/**
 * Drawing (pixel-office §3–§5, prototype section 12 after floor-qa): world px
 * on an integer scale, every colour from the palette tokens, every pixel a
 * datum (G1). Text is drawn in screen space so it stays crisp; its boxes are
 * converted back to world px for the geometry check.
 *
 * With `env.dbg` set, every element that must not collide is recorded as a
 * box (world px): labels, bubbles, boards, line segments, the rail and its
 * stops, frames, glass panes, badges, the wall line, monitors and desks. The
 * M16 gate (`geometry.ts`) checks them.
 */

import type { TeamKey } from '../lib/gen/TeamKey';
import { ART, AX, CELL, LANES, LAB, PAD, SHELF_W, STRIP_WIN_S, TRAFFIC_MS, WALL } from './constants';
import type { Metrics } from './constants';
import type { Cell, Line, Room, StripLane } from './layout';
import { areaOf, depY, doneShelf, itemPos, laneY, lineOy, openY, railStops, signY, todoShelf, trackY } from './layout';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { FlItem, FlSeat, FlState, FlTeam, FlVm, Structure } from './model';
import { BAND_OF, GLYPH_CH, WORST, bottleneckOf, currentItem, mailTo, stateCounts } from './model';
import type { Palette } from './palette';
import type { ReplayRing } from './ring';
import type { Ctx2D, Identity, Pose, SpriteCache } from './sprites';
import { drawBubble, drawGlyph, px } from './sprites';

// ---------------------------------------------------------------------------
// The environment of one frame
// ---------------------------------------------------------------------------

export interface Pt {
  x: number;
  y: number;
}

export type BoxKind = 'label' | 'bubble' | 'board' | 'seg' | 'rail' | 'stop' | 'frame' | 'pane' | 'badge' | 'wall' | 'monitor' | 'desk';

export interface Box {
  k: BoxKind;
  x: number;
  y: number;
  w: number;
  h: number;
  tag?: string;
  /** The seat or station the box belongs to. */
  id?: string;
  room?: Room;
  line?: Line;
  /** A line may leave its own board / enter its own station's frame or pane. */
  own?: string;
  zone?: 'wall' | 'floor' | 'strip';
  txt?: string;
}

export interface Walk {
  path: Pt[];
  t0: number;
  dur: number;
}

export interface Trip {
  key: string;
  it: FlItem;
  team: FlTeam;
  path: Pt[];
  t0: number;
  dur: number;
  /** The nameplate band the card passes behind (hidden there). */
  band: [number, number];
}

export interface Plane {
  from: string;
  to: string;
  t0: number;
  dur: number;
}

export interface Anim {
  walks: Map<string, Walk>;
  cards: Trip[];
  planes: Plane[];
}

export type Translate = (key: string, params?: Readonly<Record<string, string | number | boolean>>) => string;

export interface Words {
  t: Translate;
  /** A humanized duration ("12분" / "12 min"). */
  dur: (ms: number) => string;
  /** The state word of a seat (sub-case, "(추정)"). */
  state: (seat: FlSeat) => string;
}

export interface DrawEnv {
  c: Ctx2D;
  P: Palette;
  M: Metrics;
  scale: number;
  dpr: number;
  vm: FlVm;
  rooms: Room[];
  cells: Map<string, Cell>;
  /** Wall ms the frame shows. */
  nowMs: number;
  /** Monotonic seconds (animation clock). */
  rt: number;
  /** Nothing moves: reduced motion, Pause, replay or a frozen clock. */
  still: boolean;
  replay: boolean;
  anim: Anim;
  sprites: SpriteCache;
  identity: (seat: FlSeat) => Identity;
  dbg: Box[] | null;
  words: Words;
  sel: string | null;
  /** A seat highlighted from the counter or the twin (hover). */
  hl: string | null;
  traffic: boolean;
  global: boolean;
  follow: TeamKey | null;
  /** Out: character positions (head origin), for planes and routes. */
  cps: Map<string, Pt>;
  /** Out: what the scheduler needs to know. */
  flags: { typing: boolean; error: boolean; elapsed: boolean };
}

export function dbg(env: DrawEnv, k: BoxKind, x: number, y: number, w: number, h: number, o: Partial<Box> = {}): void {
  if (env.dbg) env.dbg.push({ k, x, y, w, h, ...o });
}

function dbgEdges(env: DrawEnv, k: BoxKind, x: number, y: number, w: number, h: number, o: Partial<Box> = {}): void {
  dbg(env, k, x, y, w, 1, o);
  dbg(env, k, x, y + h - 1, w, 1, o);
  dbg(env, k, x, y, 1, h, o);
  dbg(env, k, x + w - 1, y, 1, h, o);
}

/** 1-px orthogonal segment; the dot phase follows absolute coordinates so shared lanes stay in step. */
function dotLine(env: DrawEnv, x0: number, y0: number, x1: number, y1: number, step: number, o: Partial<Box>): void {
  const c = env.c;
  const h = y0 === y1;
  const a = h ? Math.min(x0, x1) : Math.min(y0, y1);
  const b = h ? Math.max(x0, x1) : Math.max(y0, y1);
  for (let v = a; v <= b; v++) {
    if (step === 1 || (step === 2 ? (v & 1) === 0 : v % 3 !== 2) || v === a || v === b) c.fillRect(h ? v : x0, h ? y0 : v, 1, 1);
  }
  dbg(env, 'seg', h ? a : x0, h ? y0 : a, h ? b - a + 1 : 1, h ? 1 : b - a + 1, o);
}

const seated = (env: DrawEnv) => (id: string) => !env.anim.walks.has(id);

export function pathLen(p: readonly Pt[]): number {
  let l = 0;
  for (let i = 1; i < p.length; i++) l += Math.abs((p[i] as Pt).x - (p[i - 1] as Pt).x) + Math.abs((p[i] as Pt).y - (p[i - 1] as Pt).y);
  return l;
}

export function alongPath(p: readonly Pt[], k: number): Pt {
  let d = Math.max(0, Math.min(1, k)) * pathLen(p);
  for (let i = 1; i < p.length; i++) {
    const p0 = p[i - 1] as Pt;
    const p1 = p[i] as Pt;
    const seg = Math.abs(p1.x - p0.x) + Math.abs(p1.y - p0.y);
    if (d <= seg) {
      const f = seg ? d / seg : 1;
      return { x: Math.round(p0.x + (p1.x - p0.x) * f), y: Math.round(p0.y + (p1.y - p0.y) * f) };
    }
    d -= seg;
  }
  return p[p.length - 1] ?? { x: 0, y: 0 };
}

/** The path a new seat walks in on: from the door, along the bubble band, down into the chair. */
export function walkPath(r: Room, cell: Cell): Pt[] {
  const d = { x: r.x + PAD, y: r.y + WALL - 16 };
  const yy = cell.y - 4;
  const x = cell.x + AX + 10;
  return [{ x: d.x, y: d.y }, { x: d.x, y: yy }, { x, y: yy }, { x, y: cell.y + 7 }];
}

// ---------------------------------------------------------------------------
// Static layer: floors, walls, doors, shelves, the rail
// ---------------------------------------------------------------------------

export function drawRooms(env: DrawEnv): void {
  const { c, P, M } = env;
  for (const r of env.rooms) {
    for (let y = r.y + WALL; y < r.y + r.h; y += 8) {
      for (let x = r.x; x < r.x + r.w; x += 8) {
        px(c, x, y, Math.min(8, r.x + r.w - x), Math.min(8, r.y + r.h - y), (((x - r.x) >> 3) + ((y - r.y - WALL) >> 3)) & 1 ? P.floorB : P.floorA);
      }
    }
    px(c, r.x, r.y, r.w, WALL, P.wall);
    px(c, r.x, r.y, r.w, 2, P.wallTop);
    px(c, r.x, r.y + WALL - 1, r.w, 1, P.trim);
    c.fillStyle = P.trim;
    c.fillRect(r.x, r.y, r.w, 1);
    c.fillRect(r.x, r.y + r.h - 1, r.w, 1);
    c.fillRect(r.x, r.y, 1, r.h);
    c.fillRect(r.x + r.w - 1, r.y, 1, r.h);
    dbg(env, 'wall', r.x, r.y + WALL - 1, r.w, 1, { tag: r.key });
    const dx = r.x + PAD;
    px(c, dx, r.y + 3, 12, WALL - 3, P.trim);
    const walkingIn = [...env.anim.walks.entries()].some(([id, w]) => env.cells.get(id)?.room === r && alongPath(w.path, (env.rt - w.t0) / w.dur).y < r.y + WALL - 6);
    px(c, dx + 1, r.y + 4, 10, WALL - 4, walkingIn ? P.doorDark : P.door);
    if (!walkingIn) px(c, dx + 8, r.y + 14, 2, 2, P.knob);
    for (const l of r.lines) {
      if (l.idx > 0) {
        // the sign row of a second team line: a trim rule across the bay, the header under it
        const sy = signY(l, M);
        px(c, r.x + 1, sy, r.w - 2, 1, P.trim);
        dbg(env, 'wall', r.x + 1, sy, r.w - 2, 1, { tag: r.key + ':sign' });
      }
      drawLineStatic(env, l);
    }
  }
}

function drawLineStatic(env: DrawEnv, l: Line): void {
  const { c, P, M } = env;
  const r = l.room;
  const ty = trackY(l, M);
  const shelves: [ReturnType<typeof todoShelf>, string][] = l.team.inferred ? [[doneShelf(l), 'done:']] : [[todoShelf(l), 'todo:'], [doneShelf(l), 'done:']];
  for (const [sh, id] of shelves) {
    px(c, sh.x, sh.y, sh.w, sh.h, P.boardEdge);
    px(c, sh.x + 1, sh.y + 1, sh.w - 2, sh.h - 2, P.board);
    dbg(env, 'board', sh.x, sh.y, sh.w, sh.h, { id: id + l.team.key, room: r, line: l });
  }
  if (!l.team.inferred) {
    const ts = todoShelf(l);
    px(c, ts.x + 1, ts.y + 18, ts.w - 2, 1, P.boardLine);
    drawGlyph(c, 'lock', ts.x + 25, ts.y + 15, P.ink);
  }
  const stops = railStops(l);
  const xa = stops[0] as number;
  const xb = stops[stops.length - 1] as number;
  px(c, xa - 1, ty, xb - xa + 2, 2, P.trim);
  stops.forEach((x, i) => {
    px(c, x - 1, ty - 1, 2, 4, P.boardEdge);
    dbg(env, 'stop', x - 1, ty - 1, 2, 4, { room: r, line: l, id: stationIdAt(l, i) });
    if (i) {
      // a direction chevron half-way to the previous stop
      const m = Math.round(((stops[i - 1] as number) + x) / 2);
      px(c, m - 1, ty - 1, 1, 1, P.boardLine);
      px(c, m, ty, 1, 2, P.boardLine);
      px(c, m - 1, ty + 2, 1, 1, P.boardLine);
    }
  });
  dbg(env, 'rail', xa - 1, ty - 1, xb - xa + 2, 4, { room: r, line: l });
}

/** The station a rail stop serves: `todo:<team>`, a member key, or `done:<team>`. */
export function stationIdAt(l: Line, i: number): string {
  const offset = l.team.inferred ? 0 : 1;
  if (!l.team.inferred && i === 0) return 'todo:' + l.team.key;
  const d = i - offset;
  if (d >= 0 && d < l.team.line.length) return l.team.line[d] as string;
  return 'done:' + l.team.key;
}

// ---------------------------------------------------------------------------
// Cards, shelves and dependencies
// ---------------------------------------------------------------------------

function cardColour(env: DrawEnv, it: FlItem): string {
  const o = it.owner && it.station !== 'todo' ? env.vm.seats.get(it.owner) : undefined;
  return o && o.colour ? env.P.id[o.colour] || env.P.info : env.P.info;
}

/** Shelf card 8 × 5, desk/tray card 6 × 2; a dotted edge = inferred; `slot` = the dashed slot of a card waiting on you. */
export function drawCard(env: DrawEnv, x: number, y: number, it: FlItem, opt: { flat?: boolean; slot?: boolean; locked?: boolean } = {}): void {
  const { c, P } = env;
  const col = cardColour(env, it);
  if (opt.slot) {
    c.fillStyle = col;
    for (let i = 0; i < 6; i += 2) {
      c.fillRect(x + i, y, 1, 1);
      c.fillRect(x + i + 1, y + 1, 1, 1);
    }
    return;
  }
  if (opt.flat) {
    px(c, x, y, 6, 2, P.paper);
    px(c, x, y, 1, 2, col);
    if (it.inferred) {
      c.fillStyle = P.paperEdge;
      c.fillRect(x + 2, y, 1, 1);
      c.fillRect(x + 4, y, 1, 1);
    }
    return;
  }
  px(c, x, y, 8, 5, P.paper);
  px(c, x, y, 2, 5, col);
  px(c, x + 3, y + 2, 4, 1, P.boardLine);
  if (it.inferred) {
    c.fillStyle = P.paperEdge;
    for (let i = 3; i < 8; i += 2) {
      c.fillRect(x + i, y, 1, 1);
      c.fillRect(x + i, y + 4, 1, 1);
    }
    c.fillRect(x + 7, y + 2, 1, 1);
  }
  if (opt.locked) drawGlyph(c, 'lock', x + 4, y, P.ink);
}

function travelling(env: DrawEnv, team: FlTeam, it: FlItem): boolean {
  return env.anim.cards.some((a) => a.key === team.key + ':' + it.key);
}

export function drawShelves(env: DrawEnv, l: Line): void {
  const { c, P, M } = env;
  const team = l.team;
  for (const it of team.items) {
    if (travelling(env, team, it)) continue;
    const p = itemPos(l, it, env.cells, seated(env));
    if (p && !p.flat) drawCard(env, p.x, p.y, it, { locked: p.ar === 'blocked' });
  }
  if (team.inferred) return; // dependencies are only drawn when declared
  // A blocked card's dependency: out of the rack along the free row under its card, down the gap between the
  // shelf and the first desk, along the dependency lane above the rail, and onto the blocker's rail stop. It never
  // enters a nameplate or a station label; several cards waiting on one desk converge on its stop.
  const ts = todoShelf(l);
  const gx = ts.x + ts.w + 1;
  const ly = depY(l, M);
  c.fillStyle = P.text;
  for (const it of team.items) {
    if (it.station !== 'blocked' || !it.blockedBy) continue;
    const bl = team.items.find((x) => x.key === it.blockedBy);
    if (!bl || !bl.owner) continue;
    if (bl.station !== 'doing' && bl.station !== 'you' && bl.station !== 'assigned') continue;
    const cc = env.cells.get(bl.owner);
    const p = itemPos(l, it, env.cells, seated(env));
    if (!cc || cc.line !== l || !p || p.flat) continue;
    const x0 = p.x + 4;
    const y0 = p.y + 5;
    const x1 = cc.x + CELL / 2 - 1;
    const o: Partial<Box> = { tag: 'dep', room: l.room, line: l };
    c.fillStyle = P.text;
    dotLine(env, x0, y0, gx, y0, 2, { ...o, own: 'todo:' + team.key });
    dotLine(env, gx, y0, gx, ly, 2, o);
    dotLine(env, gx, ly, x1, ly, 2, o);
    px(c, x1, ly + 1, 1, 1, P.text); // touches the stop
  }
}

// ---------------------------------------------------------------------------
// Seats
// ---------------------------------------------------------------------------

export function poseOf(s: FlSeat): Pose {
  if (s.state === 'needsApproval') return 'raise';
  if (s.state === 'needsAnswer') return 'tilt';
  if (s.state === 'error' || s.state === 'crashed') return 'faceplant';
  if (s.state === 'newReply') return 'lean';
  return 'sit';
}

const poseDy = (p: Pose): number => (p === 'raise' || p === 'stand' ? -3 : p === 'lean' ? -1 : 0);

type Screen = 'off' | 'broken' | 'flat' | 'dim' | 'on';

function screenKind(s: FlSeat): Screen {
  if (s.ghost) return 'off';
  if (s.state === 'error' || s.state === 'crashed') return 'broken';
  if (s.state === 'statusUnknown') return 'flat';
  if (s.state === 'ready' || s.state === 'newReply') return 'dim';
  return 'on';
}

function drawMonitor(env: DrawEnv, x: number, y: number, kind: Screen, typing: boolean): void {
  const { c, P } = env;
  px(c, x, y, 9, 7, P.monitor);
  px(c, x + 3, y + 7, 3, 1, P.monitor);
  const sx = x + 1;
  const sy = y + 1;
  const bg = kind === 'off' || kind === 'broken' || kind === 'flat' ? P.screenOff : P.screenOn;
  px(c, sx, sy, 7, 5, bg);
  if (kind === 'dim') {
    // Ready: a dimmed screen (half the lines off)
    for (let i = 0; i < 5; i += 2) px(c, sx, sy + i, 7, 1, P.screenOff);
  } else if (kind === 'on') {
    // Working: text lines that scroll only while typing (4 fps)
    const tick = typing ? Math.floor(env.rt * 4) : 0;
    for (let i = 0; i < 3; i++) px(c, sx + 1, sy + 1 + i, [5, 3, 6, 4, 2][(i + tick) % 5] as number, 1, P.screenLine);
  } else if (kind === 'flat') {
    px(c, sx + 1, sy + 2, 5, 1, P.info);
  } else if (kind === 'broken') {
    c.fillStyle = P.error;
    for (const [dx, dy] of [[0, 3], [1, 2], [2, 3], [3, 1], [4, 2], [5, 1], [6, 2]] as const) c.fillRect(sx + dx, sy + dy, 1, 1);
  }
}

function drawDesk(env: DrawEnv, sx: number, sy: number): void {
  const { c, P } = env;
  px(c, sx + 3, sy + 18, 26, 3, P.deskTop);
  px(c, sx + 3, sy + 21, 26, 7, P.desk);
  px(c, sx + 3, sy + 27, 26, 1, P.deskEdge);
}

/**
 * Seat art in a 32-px box at sx = cell.x + AX (art origin sy = cell.y): chair 8–23, character 10–21, desk 3–28
 * (centre 16 = the cell centre), monitor 2–10 at the desk's left corner, queue tray 21–28.
 */
export function drawSeat(env: DrawEnv, s: FlSeat, cell: Cell, team: FlTeam | null): Pt {
  const { c, P } = env;
  const sx = cell.x + AX;
  const sy = cell.y;
  const walking = env.anim.walks.has(s.id);
  const ghost = s.ghost;
  const cx = ghost ? sx + 15 : sx + 8;
  px(c, cx, sy + 9, 16, 9, P.chair);
  px(c, cx, sy + 9, 16, 1, P.chairDark);
  const pose = poseOf(s);
  const cp = { x: sx + 10, y: sy + 7 + poseDy(pose) };
  if (!walking) {
    const sp = env.sprites.get(P, env.identity(s), ghost ? 'sit' : pose, ghost);
    if (sp) c.drawImage(sp, cp.x - 1, cp.y - 1);
  }
  drawDesk(env, sx, sy);
  px(c, sx + 4, sy + 28, 2, 2, P.deskEdge);
  px(c, sx + 26, sy + 28, 2, 2, P.deskEdge);
  dbg(env, 'desk', sx + 3, sy + 18, 26, 12, { id: s.id, room: cell.room });
  const typing = s.state === 'working' && !env.still;
  drawMonitor(env, sx + 2, sy + 10, screenKind(s), typing);
  dbg(env, 'monitor', sx + 2, sy + 10, 9, 8, { id: s.id });
  if (!walking && !ghost && pose === 'sit' && (s.state === 'working' || s.state === 'noProgress')) {
    // hands on keys; they move only while working (No progress is frozen with hands on keys)
    const skin = env.identity(s).skin;
    const f = typing ? Math.floor(env.rt * 4) % 2 : 0;
    px(c, sx + 11, sy + 18 - f, 2, 1, skin);
    px(c, sx + 19, sy + 17 + f, 2, 1, skin);
  }
  if (team && cell.line && !walking) {
    // queue tray, current card, or the dashed slot of a card that waits on you
    px(c, sx + 21, sy + 17, 8, 1, P.deskEdge);
    for (const it of team.items) {
      if (it.owner !== s.id || travelling(env, team, it)) continue;
      const p = itemPos(cell.line, it, env.cells, seated(env));
      if (!p || !p.flat) continue;
      if (p.ar === 'desk' && it.station === 'you') {
        drawCard(env, p.x, p.y, it, { slot: true });
        drawGlyph(c, 'down', sx + 14, sy + 21, P.warn);
        dbg(env, 'badge', sx + 14, sy + 21, 5, 5, { tag: 'you', id: s.id });
      } else drawCard(env, p.x, p.y, it, { flat: true });
    }
  }
  if (s.watch) {
    // glass pane: you can watch, not type. Centred on the desk + chair box, 1 px clear of the art on every side
    c.fillStyle = P.glass;
    c.fillRect(sx, sy + 1, ART, 1);
    c.fillRect(sx, sy + 31, ART, 1);
    c.fillRect(sx, sy + 2, 1, 29);
    c.fillRect(sx + ART - 1, sy + 2, 1, 29);
    dbgEdges(env, 'pane', sx, sy + 1, ART, 31, { id: s.id });
    c.fillStyle = P.glint;
    for (let i = 0; i < 4; i++) c.fillRect(sx + 24 + i, sy + 10 - i, 1, 1);
    for (let i = 0; i < 3; i++) c.fillRect(sx + 20 + i, sy + 9 - i, 1, 1);
  }
  if (s.state === 'working' && !ghost) env.flags.typing = true;
  if (s.state === 'error' || s.state === 'crashed') env.flags.error = true;
  return cp;
}

/** The walking sprite and the desk drawn over it once it is down in its chair. */
export function drawWalker(env: DrawEnv, s: FlSeat, cell: Cell, w: Walk): Pt {
  const { c, P } = env;
  const p = alongPath(w.path, (env.rt - w.t0) / w.dur);
  const sp = env.sprites.get(P, env.identity(s), Math.floor(env.rt * 8) % 2 ? 'walk0' : 'walk1', false);
  if (sp) c.drawImage(sp, p.x - 1, p.y - 1);
  if (p.y > cell.y - 2) drawDesk(env, cell.x + AX, cell.y);
  return p;
}

/** A static dashed frame around the whole station (art + nameplate), 1 px inside the cell; never blinks. */
export function drawFrameDashed(env: DrawEnv, cell: Cell, col: string, id: string): void {
  const { c, M } = env;
  const x0 = cell.x + 1;
  const x1 = cell.x + CELL - 2;
  const y0 = cell.y - 1;
  const y1 = cell.y + M.lb;
  c.fillStyle = col;
  for (let x = x0; x <= x1; x += 2) {
    c.fillRect(x, y0, 1, 1);
    c.fillRect(x, y1, 1, 1);
  }
  for (let y = y0; y <= y1; y += 2) {
    c.fillRect(x0, y, 1, 1);
    c.fillRect(x1, y, 1, 1);
  }
  c.fillRect(x1, y0, 1, 1);
  c.fillRect(x0, y1, 1, 1);
  c.fillRect(x1, y1, 1, 1);
  dbgEdges(env, 'frame', x0, y0, x1 - x0 + 1, y1 - y0 + 1, { tag: 'bottleneck', room: cell.room, id });
}

/** The bubble sits in the band above the station, right of the head; badges sit on the desk's front panel. */
export function drawOverlays(env: DrawEnv, s: FlSeat, cp: Pt, cell: Cell, team: FlTeam | null): void {
  const { c, P } = env;
  const sx = cell.x + AX;
  const sy = cell.y;
  const bx = sx + 21;
  const by = sy - 12;
  let b = true;
  const corner = s.watch;
  if (s.state === 'needsApproval') drawBubble(c, P, bx, by, 'tri', P.warn, { corner });
  else if (s.state === 'needsAnswer') drawBubble(c, P, bx, by, 'quest', P.warn, { corner });
  else if (s.state === 'error' || s.state === 'crashed') {
    drawBubble(c, P, bx, by, 'cross', P.error, { heavy: true });
    // two grey puffs rise 1 px every 500 ms (still under reduced motion)
    const k = env.still ? 0 : Math.floor(env.rt * 2) % 3;
    [[-6, 2], [-3, -1]].forEach(([dx, dy], i) => px(c, cp.x + (dx as number), cp.y + (dy as number) - ((k + i) % 3), 2, 2, P.smoke));
  } else if (s.state === 'noProgress') drawBubble(c, P, bx, by, 'wave', P.warn, { dashed: true });
  else if (s.state === 'newReply') drawBubble(c, P, bx, by, 'dot', P.ok);
  else b = false;
  if (b) dbg(env, 'bubble', bx - 1, by - 1, 11, 11, { id: s.id, room: cell.room });
  if (b && corner && (s.state === 'needsApproval' || s.state === 'needsAnswer')) dbg(env, 'bubble', bx + 10, by, 3, 3, { id: s.id, room: cell.room, tag: 'corner' });
  const mail = team ? mailTo(team, s.id as MemberKey) : 0;
  for (let i = 0; i < Math.min(3, mail); i++) {
    // messages in flight to this member: envelopes on the front panel, right
    const x = sx + 23 - i * 6;
    const y = sy + 22;
    px(c, x, y, 5, 4, P.paper);
    drawGlyph(c, 'env', x, y, P.ink);
    dbg(env, 'badge', x, y, 5, 4, { tag: 'mail', id: s.id });
  }
  if (s.quotaLeft !== null && s.quotaLeft < 20) {
    // low quota: a battery on the front panel, left
    const x = sx + 4;
    const y = sy + 22;
    c.fillStyle = P.text;
    c.fillRect(x, y, 5, 1);
    c.fillRect(x, y + 3, 5, 1);
    c.fillRect(x, y, 1, 4);
    c.fillRect(x + 4, y, 1, 4);
    c.fillRect(x + 5, y + 1, 1, 2);
    if (s.quotaLeft >= 5) px(c, x + 1, y + 1, Math.max(1, Math.round((s.quotaLeft / 20) * 3)), 2, P.warn);
    dbg(env, 'badge', x, y, 6, 4, { tag: 'battery', id: s.id });
  }
}

// ---------------------------------------------------------------------------
// Flows
// ---------------------------------------------------------------------------

/**
 * Recent hand-offs (last 5 min): clean 1-px orthogonal routes in the lane band between the wall line (or the sign)
 * and the bubbles. A route rises from just above the sender's head (left of its centre), runs along its lane and
 * drops to just above the recipient's head (right of its centre), where a 3-px foot marks the recipient; the drops
 * pass left of the bubbles and may enter only their own station's frame. At most 3 pairs per line, newest first;
 * older ones fade; shorter spans take lower lanes so nested routes never cross. Weight = message count: dotted 1–2,
 * dashed 3–5, solid 6+.
 */
export function drawTraffic(env: DrawEnv): void {
  if (!env.traffic) return;
  const { c, P } = env;
  for (const r of env.rooms) {
    for (const l of r.lines) {
      const pairs = new Map<string, { from: string; to: string; n: number; last: number; rank: number; xs: number; xr: number }>();
      for (const m of l.team.msgs) {
        const ca = env.cells.get(m.from);
        const cb = env.cells.get(m.to);
        if (m.at > env.nowMs || env.nowMs - m.at >= TRAFFIC_MS || !ca || !cb || ca.line !== l || cb.line !== l) continue;
        const k = m.from + '>' + m.to;
        const p = pairs.get(k) ?? { from: m.from, to: m.to, n: 0, last: -Infinity, rank: 0, xs: 0, xr: 0 };
        p.n++;
        p.last = Math.max(p.last, m.at);
        pairs.set(k, p);
      }
      const shown = [...pairs.values()].sort((a, b) => b.last - a.last || (a.from + a.to < b.from + b.to ? -1 : 1)).slice(0, LANES);
      shown.forEach((p, i) => {
        p.rank = i;
        p.xs = (env.cells.get(p.from) as Cell).x + AX + 14;
        p.xr = (env.cells.get(p.to) as Cell).x + AX + 17;
      });
      const endY = (id: string) => {
        const q = env.cps.get(id);
        return q && !env.anim.walks.has(id) ? q.y - 2 : lineOy(l) - 4; // 1 px above the head's outline
      };
      shown
        .slice()
        .sort((a, b) => Math.abs(a.xr - a.xs) - Math.abs(b.xr - b.xs) || a.rank - b.rank)
        .forEach((p, lane) => {
          const y = laneY(l, lane);
          const sf = env.vm.seats.get(p.from);
          const step = p.n >= 6 ? 1 : p.n >= 3 ? 3 : 2;
          const o: Partial<Box> = { tag: 'traffic', room: r, line: l };
          const ys = endY(p.from);
          const yr = endY(p.to);
          const os = { ...o, own: p.from };
          const orr = { ...o, own: p.to };
          c.fillStyle = sf && sf.colour ? P.id[sf.colour] || P.info : P.info;
          const fade = [1, 0.7, 0.45][p.rank] ?? 0.45;
          c.globalAlpha = env.hl && env.hl !== p.from && env.hl !== p.to ? fade * 0.2 : fade;
          dotLine(env, p.xs, ys, p.xs, y, step, os);
          dotLine(env, p.xs, y, p.xr, y, step, { ...o, tag: 'traffic-lane' });
          dotLine(env, p.xr, y, p.xr, yr, step, orr);
          c.fillRect(p.xr - 1, yr, 3, 1);
          dbg(env, 'seg', p.xr - 1, yr, 3, 1, { ...orr, tag: 'traffic-foot' });
          c.globalAlpha = 1;
        });
    }
  }
}

export function drawPlanes(env: DrawEnv): void {
  const { c, P } = env;
  for (const p of env.anim.planes) {
    const pa = env.cps.get(p.from);
    const pb = env.cps.get(p.to);
    if (!pa || !pb) continue;
    const ra = env.cells.get(p.from)?.room;
    const top = ra ? ra.y + WALL + 3 : -Infinity; // the flight stays on the floor, under the wall line
    const k = Math.max(0, Math.min(1, (env.rt - p.t0) / p.dur));
    const ax = pa.x + 6;
    const ay = pa.y + 4;
    const bx = pb.x + 14;
    const by = pb.y + 4;
    const mx = (ax + bx) / 2;
    const my = Math.max(top, Math.min(ay, by) - 22);
    const x = Math.round((1 - k) * (1 - k) * ax + 2 * (1 - k) * k * mx + k * k * bx);
    const y = Math.round((1 - k) * (1 - k) * ay + 2 * (1 - k) * k * my + k * k * by);
    const sf = env.vm.seats.get(p.from);
    const dir = bx >= ax ? 1 : -1;
    c.fillStyle = sf && sf.colour ? P.id[sf.colour] || P.info : P.info;
    ['x....', 'xxx..', 'xxxxx', '.x...'].forEach((row, yy) => {
      for (let xx = 0; xx < 5; xx++) if (row[xx] === 'x') c.fillRect(dir > 0 ? x + xx - 2 : x + 2 - xx, y + yy - 2, 1, 1);
    });
    c.fillStyle = P.outline;
    c.fillRect(dir > 0 ? x + 2 : x - 2, y, 1, 1);
  }
}

/** Cards travel along the rail when their station changes; they pass behind the nameplate row (hidden there). */
export function drawCardTrips(env: DrawEnv): void {
  for (const a of env.anim.cards) {
    const p = alongPath(a.path, (env.rt - a.t0) / a.dur);
    if (p.y + 5 <= a.band[0] || p.y >= a.band[1]) drawCard(env, p.x, p.y, a.it);
  }
}

// ---------------------------------------------------------------------------
// Text (screen space)
// ---------------------------------------------------------------------------

export function fitText(c: Ctx2D, txt: string, max: number): string {
  if (c.measureText(txt).width <= max) return txt;
  let s = txt;
  while (s.length > 1 && c.measureText(s + '…').width > max) s = s.slice(0, -1);
  return s + '…';
}

interface RowItem {
  x: number;
  alts: string[];
  col: string;
}

/**
 * One row of station labels: each keeps to its share of the row (half-way to the next labelled stop), centred
 * under its stop when it fits, shifted or shortened when not.
 */
function placeRow(env: DrawEnv, items: RowItem[], lo: number, hi: number, gap: number): { txt: string; x: number; col: string }[] {
  const s = env.scale;
  const list = items.filter((it) => it.alts.length);
  const out: { txt: string; x: number; col: string }[] = [];
  list.forEach((it, i) => {
    const prev = list[i - 1];
    const next = list[i + 1];
    const L = prev ? (prev.x + it.x) / 2 + gap / 2 : lo;
    const R = next ? (it.x + next.x) / 2 - gap / 2 : hi;
    let txt = it.alts.find((a) => env.c.measureText(a).width / s <= R - L);
    if (txt === undefined) txt = fitText(env.c, it.alts[it.alts.length - 1] as string, (R - L) * s);
    const w = env.c.measureText(txt).width / s;
    out.push({ txt, x: Math.max(L + w / 2, Math.min(R - w / 2, it.x)), col: it.col });
  });
  return out;
}

/** The team header (wall or sign row). */
export function teamHeader(env: DrawEnv, team: FlTeam): string {
  const { t, dur } = env.words;
  const el = team.startedAt !== null ? dur(Math.max(0, env.nowMs - team.startedAt)) : '—';
  const done = (team.approx ? '≈' : '') + team.done;
  let head: string;
  if (team.noTaskList) head = t('floor.teamHeadNoList', { team: team.name, counts: stateCounts(env.vm, team) });
  else if (team.inferred) head = t('floor.teamHeadInferred', { team: team.name, done, all: team.total, elapsed: el });
  else if (team.source === 'orchestrator') head = t('floor.teamHeadOrchestrator', { team: team.name, done, all: team.total, elapsed: el });
  else head = t('floor.teamHead', { team: team.name, done, all: team.total, elapsed: el });
  if (team.inFlight !== null) head += t('floor.teamHeadInFlight', { flight: team.inFlight });
  return head;
}

export function drawFloorText(env: DrawEnv): void {
  const { c, P, M, scale, dpr } = env;
  const { t, dur } = env.words;
  const font = (bold: boolean, sz: number) => (bold ? '600 ' : '') + sz + 'px ' + P.sans;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.textBaseline = 'top';
  const put = (txt: string, x: number, y: number, align: CanvasTextAlign, h: number, o: Partial<Box>) => {
    c.textAlign = align;
    c.fillText(txt, x * scale, y * scale);
    const w = c.measureText(txt).width / scale;
    dbg(env, 'label', align === 'center' ? x - w / 2 : align === 'right' ? x - w : x, y, w, h, { txt, ...o });
    return w;
  };
  for (const r of env.rooms) {
    const first = r.lines[0];
    const x0 = r.x + PAD + 15;
    const inner = r.x + r.w - 4 - x0;
    const head = first ? teamHeader(env, first.team) : '';
    c.font = font(false, 11);
    const hw = head ? c.measureText(head).width / scale : 0;
    c.font = font(true, 12);
    const tw = c.measureText(r.label).width / scale;
    // the project name keeps at least 40 %; the team header takes the rest
    const tmax = !head || tw + hw + 8 <= inner ? inner : Math.min(tw, Math.max(inner * 0.4, inner - hw - 8));
    c.fillStyle = P.textStrong;
    const used = put(fitText(c, r.label, tmax * scale), x0, r.y + 5, 'left', Math.ceil(15 / scale), { room: r, zone: 'wall' });
    if (head) {
      c.font = font(false, 11);
      c.fillStyle = P.text;
      put(fitText(c, head, (inner - used - 8) * scale), r.x + r.w - 4, r.y + 6, 'right', Math.ceil(14 / scale), { room: r, zone: 'wall', tag: 'head' });
    }
    for (const l of r.lines) {
      const team = l.team;
      if (l.idx > 0) {
        c.font = font(false, 11);
        c.fillStyle = P.text;
        put(fitText(c, teamHeader(env, team), (r.w - PAD * 2) * scale), r.x + PAD, signY(l, M) + 2, 'left', Math.ceil(14 / scale), { room: r, zone: 'floor', tag: 'sign' });
      }
      // station labels, one under each rail stop
      const items: RowItem[] = [];
      if (!team.inferred) {
        const ts = todoShelf(l);
        const n = team.items.filter((x) => areaOf(l, x, seated(env)) === 'todo').length;
        const m = team.items.filter((x) => x.station === 'blocked').length;
        items.push({ x: ts.x + SHELF_W / 2, alts: [t('floor.shelf.todo', { todo: n, blocked: m }), t('floor.shelf.todoShort', { todo: n, blocked: m })], col: P.text });
      }
      for (const id of team.line) {
        const cl = env.cells.get(id) as Cell;
        const bn = bottleneckOf(team, id as MemberKey);
        const cur = currentItem(team, id as MemberKey);
        let txt = '';
        let col = P.text;
        if (bn) {
          txt = t('floor.bottleneckN', { count: bn.n });
          col = P.warn;
        } else if (cur && cur.station === 'you') {
          txt = t('floor.stn.you');
          col = P.warn;
        } else if (cur && cur.started !== null) {
          txt = (cur.approx ? '≥ ' : '') + dur(Math.max(0, env.nowMs - cur.started));
          env.flags.elapsed = true;
        }
        items.push({ x: cl.x + CELL / 2, alts: txt ? [txt] : [], col });
      }
      const ds = doneShelf(l);
      items.push({
        x: ds.x + SHELF_W / 2,
        alts: team.inferred ? [t('floor.shelf.reported', { done: team.done }), t('floor.shelf.reportedShort', { done: team.done })] : [t('floor.shelf.done', { done: team.done })],
        col: P.text,
      });
      c.font = font(false, 10);
      for (const p of placeRow(env, items, r.x + 2, r.x + r.w - 2, 6 / scale)) {
        c.fillStyle = p.col;
        put(p.txt, p.x, trackY(l, M) + 4, 'center', M.t2, { room: r, zone: 'floor', line: l, tag: 'station' });
      }
    }
  }
  for (const [id, cl] of env.cells) {
    // nameplate, the same two lines on every seat: state glyph + name / state word
    const s = env.vm.seats.get(id);
    if (!s) continue;
    const hot = (BAND_OF[s.state] ?? 9) <= 2;
    const max = (CELL - 4) * scale;
    const cx = cl.x + CELL / 2;
    const y = cl.y + LAB;
    const o: Partial<Box> = { room: cl.room, zone: 'floor', id, tag: 'nameplate' };
    c.font = font(hot, 11);
    c.fillStyle = hot ? P.warn : id === env.sel ? P.textStrong : P.text;
    put(fitText(c, GLYPH_CH[s.state] + ' ' + s.name, max), cx, y, 'center', M.t1, { ...o, tag: 'nameplate1' });
    c.font = font(false, 10);
    c.fillStyle = P.text;
    put(fitText(c, env.words.state(s), max), cx, y + M.t1, 'center', M.t2, { ...o, tag: 'nameplate2' });
  }
}

// ---------------------------------------------------------------------------
// The flow strip (the last 10 minutes)
// ---------------------------------------------------------------------------

export interface StripGeom {
  /** CSS px within the canvas. */
  x0: number;
  x1: number;
  y0: number;
  y1: number;
  T0: number;
  T1: number;
}

type Paint = [string, number] | null;

function stripStyle(P: Palette, st: FlState | null): Paint {
  switch (st) {
    case 'working':
      return [P.textStrong, 2];
    case 'needsApproval':
    case 'needsAnswer':
      return [P.warn, 4];
    case 'error':
    case 'crashed':
      return [P.error, 4];
    case 'noProgress':
      return [P.warn, 1];
    case 'newReply':
      return [P.ok, 1];
    case 'ready':
    case 'starting':
      return [P.info, 1];
    default:
      return null;
  }
}

export interface StripData {
  /** Per lane: [from, to, state] segments. */
  segs: [number, number, FlState | null][][];
  /** "you" lane: [from, to, waiting count]. */
  you: [number, number, number][];
  ticks: { t: number; lane: number }[];
  done: number;
}

/** Sweep the ring across [T0, T1] into lane segments, waits and finished-task ticks. */
export function stripData(vm: FlVm, lanes: readonly StripLane[], ring: ReplayRing, T0: number, T1: number): StripData {
  const n = lanes.length;
  const segs: [number, number, FlState | null][][] = lanes.map(() => []);
  const you: [number, number, number][] = [];
  const ticks: { t: number; lane: number }[] = [];
  const laneOfUid = new Map<string, number[]>();
  const laneOfMember = new Map<string, number>();
  lanes.forEach((l, i) => {
    for (const id of l.members) {
      laneOfMember.set(id, i);
      const uid = vm.seats.get(id)?.uid;
      if (uid) laneOfUid.set(uid, [...(laneOfUid.get(uid) ?? []), i]);
    }
  });
  const youIdx = lanes.findIndex((l) => l.you);
  const laneState = (i: number, S: Structure): FlState | null => {
    const states: FlState[] = [];
    for (const id of (lanes[i] as StripLane).members) {
      const uid = vm.seats.get(id)?.uid;
      const f = uid ? S.sessions.get(uid) : undefined;
      if (f && (f.alive || f.kind === 'crashed')) states.push(f.kind);
    }
    return WORST.find((w) => states.includes(w)) ?? null;
  };
  const cur: (FlState | null)[] = new Array(n).fill(null);
  const from: number[] = new Array(n).fill(T0);
  let youN = 0;
  let youFrom = T0;
  let done = 0;
  ring.sweep(T0, T1, (t, S, ev, prev) => {
    if (!ev) {
      for (let i = 0; i < n; i++) if (i !== youIdx) cur[i] = laneState(i, S);
      youN = S.waits.length;
      // the first bar starts at the moment the state began, when that is inside the window
      for (let i = 0; i < n; i++) {
        let since = T0;
        for (const id of (lanes[i] as StripLane).members) {
          const uid = vm.seats.get(id)?.uid;
          const f = uid ? S.sessions.get(uid) : undefined;
          if (f && f.kind === cur[i]) since = Math.max(since, Math.min(t, f.since));
        }
        from[i] = Math.max(T0, Math.min(since, t));
      }
      youFrom = T0;
      return;
    }
    if (ev.k === 's') {
      for (const i of laneOfUid.get(ev.id) ?? []) {
        const st = laneState(i, S);
        if (st !== cur[i]) {
          (segs[i] as [number, number, FlState | null][]).push([from[i] as number, t, cur[i] ?? null]);
          cur[i] = st;
          from[i] = t;
        }
      }
    } else if (ev.k === 'w') {
      if (ev.v.length !== youN) {
        you.push([youFrom, t, youN]);
        youN = ev.v.length;
        youFrom = t;
      }
    } else if (ev.k === 't' && ev.v && (ev.v.station === 'done' || ev.v.station === 'reported')) {
      const was = prev as { station: string } | null;
      if (was && was.station !== 'done' && was.station !== 'reported' && t >= T0) {
        done++;
        const lane = ev.v.owner ? laneOfMember.get(ev.v.owner) : undefined;
        if (lane !== undefined) ticks.push({ t, lane });
      }
    }
  });
  for (let i = 0; i < n; i++) if (i !== youIdx) (segs[i] as [number, number, FlState | null][]).push([from[i] as number, T1, cur[i] ?? null]);
  you.push([youFrom, T1, youN]);
  return { segs, you, ticks, done };
}

export function drawStrip(env: DrawEnv, world: { w: number; h: number }, lanes: readonly StripLane[], data: StripData, H: number, replayT: number | null, T1: number): StripGeom {
  const { c, P, M, scale, dpr } = env;
  const pitch = M.pitch;
  const y0 = world.h;
  const W = world.w;
  const LBL = 34;
  const x0 = LBL + 2;
  const x1 = W - 4;
  const T0 = T1 - STRIP_WIN_S * 1000;
  const tx = (tt: number) => Math.round(x0 + (x1 - x0) * ((Math.max(T0, Math.min(T1, tt)) - T0) / (STRIP_WIN_S * 1000)));
  c.setTransform(scale * dpr, 0, 0, scale * dpr, 0, 0);
  c.imageSmoothingEnabled = false;
  px(c, 0, y0, W, H, P.floorA);
  px(c, 0, y0, W, 1, P.trim);
  const ly = (i: number) => y0 + 9 + i * pitch;
  lanes.forEach((_l, i) => px(c, x0, ly(i) + 3, x1 - x0, 1, P.floorB));
  lanes.forEach((l, i) => {
    if (l.you) {
      for (const [a0, b0, k] of data.you) {
        if (!k || b0 < T0) continue;
        const a = tx(a0);
        const b = tx(b0);
        const h = Math.min(4, k * 2);
        px(c, a, ly(i) + 4 - h, Math.max(1, b - a), h, P.warn);
      }
      return;
    }
    for (const [a0, b0, st] of data.segs[i] ?? []) {
      const s = stripStyle(P, st);
      if (!s || b0 <= T0 || a0 >= T1) continue;
      const a = tx(a0);
      const b = tx(b0);
      px(c, a, ly(i) + 4 - s[1], Math.max(1, b - a), s[1], s[0]);
    }
  });
  // hand-offs: a stroke from the sender's lane to the recipient's lane in the sender's colour
  const laneOf = (id: string) => lanes.findIndex((l) => l.members.includes(id));
  for (const team of env.vm.teams) {
    for (const m of team.msgs) {
      if (m.at < T0 || m.at > T1) continue;
      const i = laneOf(m.from);
      const j = laneOf(m.to);
      if (i < 0 || j < 0) continue;
      const x = tx(m.at);
      const sf = env.vm.seats.get(m.from);
      const col = sf && sf.colour ? P.id[sf.colour] || P.info : P.info;
      if (i === j) {
        px(c, x, ly(i), 1, 3, col);
        continue;
      }
      const ya = ly(i) + 2;
      const yb = ly(j) + 2;
      px(c, x, Math.min(ya, yb), 1, Math.abs(yb - ya) + 1, col);
      px(c, x - 1, yb, 3, 1, col);
    }
  }
  for (const k of data.ticks) {
    const x = tx(k.t);
    px(c, x, ly(k.lane) - 1, 1, 5, P.ok);
    px(c, x - 1, ly(k.lane) - 1, 3, 1, P.ok);
  }
  if (replayT !== null) {
    const xr = tx(replayT);
    c.globalAlpha = 0.55;
    px(c, xr + 1, y0 + 8, x1 - xr, H - 9, P.bg);
    c.globalAlpha = 1;
    px(c, xr, y0 + 7, 1, H - 8, P.accent);
  }
  px(c, x1, y0 + 7, 1, H - 8, P.textStrong);
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.textBaseline = 'middle';
  c.textAlign = 'left';
  c.font = '10px ' + P.sans;
  const lbl = (txt: string, y: number) => {
    c.fillText(txt, 6, y * scale);
    const w = c.measureText(txt).width / scale;
    const h = Math.ceil(12 / scale);
    dbg(env, 'label', 6 / scale, y - h / 2, w, h, { txt, zone: 'strip' });
  };
  c.fillStyle = P.text;
  lbl(fitText(c, env.words.t('floor.strip', { done: data.done }), (W - 8) * scale), y0 + 4);
  lanes.forEach((l, i) => {
    c.fillStyle = l.you ? P.warn : P.text;
    lbl(fitText(c, l.label, LBL * scale - 8), ly(i) + 2);
  });
  return { x0: x0 * scale, x1: x1 * scale, y0: y0 * scale, y1: (y0 + H) * scale, T0, T1 };
}

// ---------------------------------------------------------------------------
// Selection, follow, replay banner
// ---------------------------------------------------------------------------

/** The 1-px accent focus frame: the whole station, just outside the bottleneck frame. */
export function drawSelection(env: DrawEnv, id: string, dashed: boolean): void {
  const cl = env.cells.get(id);
  if (!cl) return;
  const { c, P, M } = env;
  const y0 = cl.y - 2;
  const h = M.lb + 4;
  c.fillStyle = P.accent;
  if (dashed) {
    for (let x = cl.x; x < cl.x + CELL; x += 2) {
      c.fillRect(x, y0, 1, 1);
      c.fillRect(x, y0 + h - 1, 1, 1);
    }
    for (let y = y0; y < y0 + h; y += 2) {
      c.fillRect(cl.x, y, 1, 1);
      c.fillRect(cl.x + CELL - 1, y, 1, 1);
    }
  } else {
    c.fillRect(cl.x, y0, CELL, 1);
    c.fillRect(cl.x, y0 + h - 1, CELL, 1);
    c.fillRect(cl.x, y0, 1, h);
    c.fillRect(cl.x + CELL - 1, y0, 1, h);
  }
  dbgEdges(env, 'frame', cl.x, y0, CELL, h, { tag: dashed ? 'highlight' : 'selection', room: cl.room, id });
}

/** Follow: everything but the followed line dims, its own bay's open row included. */
export function drawFollowDim(env: DrawEnv): void {
  if (!env.follow || !env.global) return;
  const { c, P, M, scale, dpr } = env;
  c.setTransform(scale * dpr, 0, 0, scale * dpr, 0, 0);
  c.globalAlpha = 0.6;
  c.fillStyle = P.bg;
  for (const r of env.rooms) {
    const mine = r.lines.find((l) => l.team.key === env.follow);
    if (!mine) {
      c.fillRect(r.x, r.y, r.w, r.h);
      continue;
    }
    for (const l of r.lines) if (l !== mine) c.fillRect(r.x + 1, l.y - (l.idx > 0 ? M.signH : 0), r.w - 2, M.lineH + (l.idx > 0 ? M.signH : 0));
    if (r.rows) c.fillRect(r.x + 1, openY(r, M), r.w - 2, r.y + r.h - 1 - openY(r, M));
  }
  c.globalAlpha = 1;
}

export function drawReplayBanner(env: DrawEnv, text: string): void {
  const { c, P, dpr } = env;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.font = '600 12px ' + P.sans;
  c.textAlign = 'left';
  c.textBaseline = 'top';
  const w = c.measureText(text).width;
  px(c, 8, 8, w + 16, 22, P.bubble);
  px(c, 8, 8, 2, 22, P.accent);
  c.fillStyle = P.textStrong;
  c.fillText(text, 16, 12);
}
