/**
 * The Floor's space (pixel-office §3): a bay per project, a team line per team
 * under the wall (a second team of the same bay gets its own sign row and
 * line below the first), standalone desks in an open grid below the lines.
 *
 * Places stay put (G4, I1–I5): a standalone seat keeps its index while the
 * Floor lives (`SeatBook`), a bay's column count is fixed on first sight, line
 * order is join order, and stations never move. Compaction happens only on
 * Tidy (`T`) or after ≥ 60 s hidden, which clear the book.
 *
 * Everything here is pure geometry in world px (no canvas), so the M16 rules
 * can be checked without a browser.
 */

import type { Origin } from '../lib/gen/Origin';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { TeamKey } from '../lib/gen/TeamKey';
import { AX, BUB_BAND, CELL, LANES, LANE_BAND, LANE_GAP, MARGIN, PAD, SHELF_GAP, SHELF_H, SHELF_W, STRIP_LANES, WALL } from './constants';
import type { Metrics } from './constants';
import type { FlItem, FlTeam, FlVm } from './model';

export type FloorScope = { kind: 'global' } | { kind: 'team'; teamKey: TeamKey };

export interface Line {
  team: FlTeam;
  /** Index of this line in its bay. */
  idx: number;
  /** Top of the line's lane band. */
  y: number;
  room: Room;
}

export interface Room {
  /** Seat-book key: scope + project. */
  key: string;
  projectId: ProjectId | null;
  label: string;
  x: number;
  y: number;
  w: number;
  h: number;
  lines: Line[];
  seats: { id: string; idx: number }[];
  cols: number;
  rows: number;
}

export interface Cell {
  id: string;
  /** Left edge of the station cell (CELL wide). */
  x: number;
  /** Art origin (top of the seat art). */
  y: number;
  room: Room;
  line: Line | null;
}

/** Stable seats and bay columns (G4). */
export class SeatBook {
  readonly seats = new Map<string, Map<string, number>>();
  readonly cols = new Map<string, number>();

  seatOf(room: string, id: string): number {
    let m = this.seats.get(room);
    if (!m) {
      m = new Map();
      this.seats.set(room, m);
    }
    const have = m.get(id);
    if (have !== undefined) return have;
    const used = new Set(m.values());
    let i = 0;
    while (used.has(i)) i++;
    m.set(id, i);
    return i;
  }

  /** Tidy (I4): forget every seat, so the next layout packs from scratch. */
  clear(): void {
    this.seats.clear();
    this.cols.clear();
  }
}

export const NO_PROJECT = '__none__';

/**
 * Apps whose desks with no project get a bay of their own, before "no
 * project": their codex servers run in `/` and serve the app, not a folder.
 */
export const APP_BAYS: readonly Origin[] = ['chatGptApp', 'vsCode'];

export const appBay = (origin: Origin): string => `__app:${origin}__`;

/** The app of an app bay's id (or seat-book key), else `null`. */
export function appOfBay(id: string): Origin | null {
  return APP_BAYS.find((o) => id.endsWith(appBay(o))) ?? null;
}

/** A desk's bay: its project when known, else its app's bay, else "no project". */
export function bayOf(known: ReadonlySet<string>, projectId: ProjectId | null, origin: Origin | null = null): string {
  if (projectId && known.has(projectId)) return projectId;
  return origin && APP_BAYS.includes(origin) ? appBay(origin) : NO_PROJECT;
}

export function lineWidth(team: FlTeam): number {
  return PAD * 2 + (team.inferred ? 0 : SHELF_W + SHELF_GAP) + team.line.length * CELL + SHELF_GAP + SHELF_W;
}

export interface SceneOptions {
  scope: FloorScope;
  /** Bay label for sessions and teams with no known project. */
  noProjectLabel: string;
  /** Bay label of an app's desks with no project (`APP_BAYS`). */
  appLabel: (origin: Origin) => string;
}

/** Bays with their lines and seats (unplaced: x = y = 0). */
export function buildRooms(vm: FlVm, M: Metrics, book: SeatBook, opts: SceneOptions): Room[] {
  const global = opts.scope.kind === 'global';
  const scopeKey = opts.scope.kind === 'team' ? opts.scope.teamKey : null;
  const teams = global ? vm.teams.filter((t) => t.alive) : vm.teams.filter((t) => t.key === scopeKey);
  const known = new Set<string>(vm.projects.map((p) => p.id));
  const order: { id: string; label: string }[] = vm.projects.map((p) => ({ id: p.id as string, label: p.name }));
  for (const o of APP_BAYS) order.push({ id: appBay(o), label: opts.appLabel(o) });
  order.push({ id: NO_PROJECT, label: opts.noProjectLabel });
  const rooms: Room[] = [];
  for (const p of order) {
    const lineTeams = teams.filter((t) => bayOf(known, t.projectId) === p.id);
    const solo = global
      ? vm.solo.filter((id) => {
          const seat = vm.seats.get(id);
          return bayOf(known, seat?.projectId ?? null, seat?.origin ?? null) === p.id;
        })
      : [];
    if (lineTeams.length === 0 && solo.length === 0) continue;
    const key = (global ? 'g:' : 't:') + p.id;
    const seats = solo.map((id) => ({ id, idx: book.seatOf(key, id) }));
    const lineW = Math.max(0, ...lineTeams.map(lineWidth));
    let cols = book.cols.get(key);
    if (cols === undefined) {
      // Fixed on first sight so seats never move; uses the width the team line already takes, and one row for a few desks.
      const base = Math.max(3, Math.min(8, Math.ceil(Math.sqrt(Math.max(1, solo.length) * 1.6))));
      cols = Math.max(base, Math.floor((lineW - PAD * 2) / CELL), solo.length <= 5 ? solo.length : 0);
      book.cols.set(key, cols);
    }
    const rows = seats.length ? Math.ceil((Math.max(...seats.map((s) => s.idx)) + 1) / cols) : 0;
    const linesH = lineTeams.length ? lineTeams.length * M.lineH + (lineTeams.length - 1) * M.signH : 0;
    const room: Room = {
      key,
      projectId: known.has(p.id) ? (p.id as ProjectId) : null,
      label: p.label,
      x: 0,
      y: 0,
      w: Math.max(lineW, PAD * 2 + cols * CELL),
      h: WALL + linesH + rows * M.openH + PAD,
      lines: [],
      seats,
      cols,
      rows,
    };
    room.lines = lineTeams.map((team, idx) => ({ team, idx, y: 0, room }));
    rooms.push(room);
  }
  return rooms;
}

/** Shelf-pack the bays left to right into `wW` world px; sets each bay's x/y and its lines' y. */
export function packRooms(rooms: Room[], wW: number, M: Metrics): { w: number; h: number } {
  let x = MARGIN;
  let y = MARGIN;
  let rowH = 0;
  let maxX = 0;
  for (const r of rooms) {
    if (x > MARGIN && x + r.w + MARGIN > wW) {
      x = MARGIN;
      y += rowH + MARGIN;
      rowH = 0;
    }
    r.x = x;
    r.y = y;
    r.lines.forEach((l, i) => {
      l.y = r.y + WALL + i * (M.lineH + M.signH);
    });
    x += r.w + MARGIN;
    rowH = Math.max(rowH, r.h);
    maxX = Math.max(maxX, x);
  }
  return { w: maxX, h: y + rowH + MARGIN };
}

// ---------------------------------------------------------------------------
// Positions (world px)
// ---------------------------------------------------------------------------

/** Left edge of the line's first desk cell. */
export const deskX0 = (l: Line): number => l.room.x + PAD + (l.team.inferred ? 0 : SHELF_W + SHELF_GAP);
/** Art origin of the line's desks. */
export const lineOy = (l: Line): number => l.y + LANE_BAND + BUB_BAND;
/** A traffic lane (0 is the lowest). */
export const laneY = (l: Line, i: number): number => l.y + 2 + (LANES - 1 - i) * LANE_GAP;
export const laneBand = (l: Line): { y0: number; y1: number } => ({ y0: l.y, y1: l.y + LANE_BAND });
export const todoShelf = (l: Line) => ({ x: l.room.x + PAD, y: lineOy(l) + 30 - SHELF_H, w: SHELF_W, h: SHELF_H });
export const doneShelf = (l: Line) => ({ x: deskX0(l) + l.team.line.length * CELL + SHELF_GAP, y: lineOy(l) + 30 - SHELF_H, w: SHELF_W, h: SHELF_H });
export const depY = (l: Line, M: Metrics): number => lineOy(l) + M.lb + 3;
/** The rail is 2 px; its stops reach 1 px above and below. */
export const trackY = (l: Line, M: Metrics): number => lineOy(l) + M.lb + 6;
/** Top of the open grid. */
export const openY = (r: Room, M: Metrics): number => r.y + WALL + (r.lines.length ? r.lines.length * M.lineH + (r.lines.length - 1) * M.signH : 0);
/** Top of the sign row above line `l` (lines after the first). */
export const signY = (l: Line, M: Metrics): number => l.y - M.signH;

/** One stop per station: the To do shelf (real task lists only), each desk, the Done shelf (centres). */
export function railStops(l: Line): number[] {
  const xs: number[] = [];
  if (!l.team.inferred) xs.push(todoShelf(l).x + SHELF_W / 2);
  l.team.line.forEach((_id, i) => xs.push(deskX0(l) + i * CELL + CELL / 2));
  xs.push(doneShelf(l).x + SHELF_W / 2);
  return xs;
}

/** Every drawn seat's cell. */
export function cellsOf(rooms: readonly Room[], M: Metrics): Map<string, Cell> {
  const cells = new Map<string, Cell>();
  for (const r of rooms) {
    for (const l of r.lines) {
      l.team.line.forEach((id, i) => cells.set(id, { id, x: deskX0(l) + i * CELL, y: lineOy(l), room: r, line: l }));
    }
    const oy = openY(r, M);
    for (const s of r.seats) cells.set(s.id, { id: s.id, x: r.x + PAD + (s.idx % r.cols) * CELL, y: oy + Math.floor(s.idx / r.cols) * M.openH + BUB_BAND, room: r, line: null });
  }
  return cells;
}

/** Where a card is drawn: on a shelf (8 × 5), or flat at a desk / in a tray (6 × 2). */
export type Area = 'todo' | 'blocked' | 'done' | 'desk' | 'tray';

export interface ItemPos {
  x: number;
  y: number;
  flat: boolean;
  ar: Area;
}

/** The area of an item, given which owners are seated (not walking in) on this line. */
export function areaOf(l: Line, it: FlItem, seated: (id: string) => boolean): Area {
  const st = it.station;
  if (st === 'done' || st === 'blocked') return st;
  const here = !!it.owner && l.team.line.includes(it.owner) && seated(it.owner);
  if ((st === 'doing' || st === 'you') && here) return 'desk';
  if (st === 'assigned' && here) return 'tray';
  return l.team.inferred ? 'tray' : 'todo';
}

/** Shelf capacity per area (cards beyond it share the last slot; the label carries the count). */
const SHELF_ROWS: Readonly<Record<'todo' | 'blocked' | 'done', [number, number]>> = { todo: [0, 1], blocked: [3, 4], done: [0, 4] };

export function itemPos(l: Line, it: FlItem, cells: ReadonlyMap<string, Cell>, seated: (id: string) => boolean): ItemPos | null {
  const ar = areaOf(l, it, seated);
  if (ar === 'desk' || ar === 'tray') {
    const c = it.owner ? cells.get(it.owner) : undefined;
    if (!c) return null;
    const mine = l.team.items.filter((x) => x.owner === it.owner && areaOf(l, x, seated) === ar);
    const i = mine.indexOf(it);
    const sx = c.x + AX;
    return ar === 'desk' ? { x: sx + 13, y: c.y + 17 - Math.min(2, i) * 2, flat: true, ar } : { x: sx + 22, y: c.y + 16 - Math.min(3, i) * 2, flat: true, ar };
  }
  if (ar === 'todo' && l.team.inferred) return null;
  const list = l.team.items.filter((x) => areaOf(l, x, seated) === ar);
  const i = list.indexOf(it);
  const sh = ar === 'done' ? doneShelf(l) : todoShelf(l);
  const [r0, r1] = SHELF_ROWS[ar];
  const slots = (r1 - r0 + 1) * 3;
  const k = Math.min(i, slots - 1);
  const row = r0 + Math.floor(k / 3);
  return { x: sh.x + 3 + (k % 3) * 10, y: sh.y + 3 + row * 6, flat: false, ar };
}

// ---------------------------------------------------------------------------
// The flow strip's lanes
// ---------------------------------------------------------------------------

export interface StripLane {
  label: string;
  you: boolean;
  /** Member keys (team lanes: every member) whose worst state the lane draws. */
  members: string[];
}

/** Global: one lane per team; a Team Floor or a followed team: one lane per member; always "you" last. */
export function stripLanes(vm: FlVm, scope: FloorScope, follow: TeamKey | null, youLabel: string): StripLane[] {
  const teamKey = scope.kind === 'team' ? scope.teamKey : follow && vm.teams.some((t) => t.key === follow) ? follow : null;
  let lanes: StripLane[];
  if (teamKey === null) {
    lanes = vm.teams.filter((t) => t.alive).map((t) => ({ label: t.name, you: false, members: t.line.slice() }));
  } else {
    const t = vm.teams.find((x) => x.key === teamKey);
    lanes = t ? t.line.filter((id) => vm.seats.has(id)).map((id) => ({ label: vm.seats.get(id)?.name ?? id, you: false, members: [id] })) : [];
  }
  return lanes.slice(0, STRIP_LANES).concat([{ label: youLabel, you: true, members: [] }]);
}

/** A title row, one lane per row, 3 px below. */
export const stripHeight = (M: Metrics, lanes: number): number => 9 + lanes * M.pitch + 3;
