/**
 * The M16 geometry check (contract §14.3, A9.2): what `__flOverlaps()` returns.
 *
 * It reads the boxes one frame recorded (`draw.ts`, world px) and reports:
 *  - overlap: pairs that must never touch — text vs text, bubbles, boards,
 *    lines, the rail and its stops, frames, glass panes, badges, desks and the
 *    wall line; bubbles vs bubbles, lines, boards, frames, the wall line,
 *    badges, panes and the rail; lines vs boards, the wall line, frames, badges
 *    and panes; badges vs badges, monitors and frames. A dependency line may
 *    leave its own board; a hand-off may enter its own station's frame or pane
 *    (never its bubble).
 *  - outside: bubbles, labels, lines and frames stay on their bay's floor;
 *    wall text stays in the wall band.
 *  - nameplate: every seat has exactly two nameplate lines (glyph + name /
 *    state word), each inside its cell.
 *  - rail: one stop per station (To do shelf, each desk, Done shelf), each
 *    desk's stop at its cell's centre.
 *  - route: a hand-off's horizontal run lies in its line's lane band, and its
 *    ends are above the speaking agents' heads, inside their cells.
 *  - subpixel: art boxes sit on whole art pixels.
 * The DOM part (nothing overlaps the counter) is added by the engine.
 */

import { CELL, LANE_BAND, WALL } from './constants';
import type { Box, BoxKind } from './draw';
import type { Cell, Room } from './layout';

export interface Overlap {
  a: string;
  b: string;
  kind: string;
}

const CLASH: Readonly<Partial<Record<BoxKind, readonly BoxKind[]>>> = {
  label: ['label', 'bubble', 'board', 'seg', 'rail', 'stop', 'frame', 'pane', 'badge', 'wall', 'desk'],
  bubble: ['bubble', 'seg', 'board', 'frame', 'wall', 'badge', 'pane', 'rail'],
  seg: ['board', 'wall', 'frame', 'badge', 'pane'],
  badge: ['badge', 'monitor', 'frame'],
};

const OWN: readonly BoxKind[] = ['board', 'frame', 'pane'];

export function boxName(a: Box): string {
  return (
    a.k +
    (a.tag ? ':' + a.tag : '') +
    (a.txt ? ' "' + a.txt + '"' : '') +
    (a.id ? ' ' + a.id : '') +
    ' @' +
    Math.round(a.x * 10) / 10 +
    ',' +
    Math.round(a.y * 10) / 10 +
    '+' +
    Math.round(a.w * 10) / 10 +
    'x' +
    Math.round(a.h * 10) / 10
  );
}

const hit = (a: Box, b: Box) => a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;

function clashes(a: Box, b: Box): boolean {
  return (CLASH[a.k] ?? []).includes(b.k) || (CLASH[b.k] ?? []).includes(a.k);
}

/** Every rule above, on one frame's boxes and cells. */
export function checkGeometry(boxes: readonly Box[], cells: ReadonlyMap<string, Cell>, rooms: readonly Room[]): Overlap[] {
  const out: Overlap[] = [];
  // overlap (a sweep over x keeps this fast for hundreds of boxes)
  const list = boxes.filter((b) => CLASH[b.k] || Object.values(CLASH).some((v) => v?.includes(b.k)));
  const sorted = list.slice().sort((a, b) => a.x - b.x);
  for (let i = 0; i < sorted.length; i++) {
    const a = sorted[i] as Box;
    for (let j = i + 1; j < sorted.length; j++) {
      const b = sorted[j] as Box;
      if (b.x >= a.x + a.w) break;
      if (!clashes(a, b)) continue;
      if ((a.zone === 'strip') !== (b.zone === 'strip')) continue; // the strip has its own space below the floor
      if ((a.own && a.own === b.id && OWN.includes(b.k)) || (b.own && b.own === a.id && OWN.includes(a.k))) continue;
      if (hit(a, b)) out.push({ a: boxName(a), b: boxName(b), kind: 'overlap' });
    }
  }
  // containment
  for (const a of boxes) {
    if (!a.room || !['bubble', 'label', 'seg', 'frame'].includes(a.k)) continue;
    const r = a.room;
    const wall = a.zone === 'wall';
    const top = wall ? r.y + 1 : r.y + WALL;
    const bot = wall ? r.y + WALL - 1 : r.y + r.h - 1;
    if (a.x < r.x + 1 || a.x + a.w > r.x + r.w - 1 || a.y < top || a.y + a.h > bot) out.push({ a: boxName(a), b: r.key, kind: 'outside' });
  }
  // two-line nameplates, inside their cells
  const plates = new Map<string, Box[]>();
  for (const b of boxes) if (b.k === 'label' && (b.tag === 'nameplate1' || b.tag === 'nameplate2') && b.id) plates.set(b.id, [...(plates.get(b.id) ?? []), b]);
  for (const [id, cell] of cells) {
    const p = plates.get(id) ?? [];
    const l1 = p.filter((b) => b.tag === 'nameplate1');
    const l2 = p.filter((b) => b.tag === 'nameplate2');
    if (l1.length !== 1 || l2.length !== 1) out.push({ a: 'nameplate ' + id, b: `${l1.length}+${l2.length} lines`, kind: 'nameplate' });
    for (const b of p) if (b.x < cell.x - 0.01 || b.x + b.w > cell.x + CELL + 0.01) out.push({ a: boxName(b), b: 'cell ' + id, kind: 'nameplate' });
    const [a1, a2] = [l1[0], l2[0]];
    if (a1 && a2 && a2.y < a1.y + a1.h) out.push({ a: boxName(a1), b: boxName(a2), kind: 'nameplate' });
  }
  // one rail stop per station, each desk's at its cell centre
  for (const r of rooms) {
    for (const l of r.lines) {
      const stops = boxes.filter((b) => b.k === 'stop' && b.line === l);
      const want = (l.team.inferred ? 0 : 1) + l.team.line.length + 1;
      if (stops.length !== want) out.push({ a: 'rail ' + l.team.key, b: `${stops.length} stops for ${want} stations`, kind: 'rail' });
      const xs = new Set<number>();
      for (const s of stops) {
        const cx = s.x + 1;
        if (xs.has(cx)) out.push({ a: boxName(s), b: 'rail ' + l.team.key, kind: 'rail' });
        xs.add(cx);
        const cell = s.id ? cells.get(s.id) : undefined;
        if (cell && cx !== cell.x + CELL / 2) out.push({ a: boxName(s), b: 'cell ' + s.id, kind: 'rail' });
      }
      for (const id of l.team.line) if (!stops.some((s) => s.id === id)) out.push({ a: 'desk ' + id, b: 'rail ' + l.team.key, kind: 'rail' });
    }
  }
  // hand-off routes: the run in the lane band, the ends above the speaking agents
  for (const b of boxes) {
    if (b.k !== 'seg' || !b.line) continue;
    if (b.tag === 'traffic-lane') {
      if (b.h !== 1 || b.y < b.line.y || b.y >= b.line.y + LANE_BAND) out.push({ a: boxName(b), b: 'lane band ' + b.line.team.key, kind: 'route' });
    }
    if ((b.tag === 'traffic' || b.tag === 'traffic-foot') && b.own) {
      const cell = cells.get(b.own);
      if (!cell) out.push({ a: boxName(b), b: 'no seat ' + b.own, kind: 'route' });
      // the end sits 1 px above the head's outline (the head's top is at most 6 px below the art origin)
      else if (b.x < cell.x || b.x + b.w > cell.x + CELL || b.y + b.h > cell.y + 6) out.push({ a: boxName(b), b: 'seat ' + b.own, kind: 'route' });
    }
  }
  // whole art pixels
  for (const b of boxes) {
    if (b.k === 'label') continue;
    if (![b.x, b.y, b.w, b.h].every(Number.isInteger)) out.push({ a: boxName(b), b: 'pixel grid', kind: 'subpixel' });
  }
  return out;
}
