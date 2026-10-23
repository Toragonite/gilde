/**
 * The M16 geometry rules in Node (contract §14.3; the real-browser run is
 * T19's `browser-gates.mjs --gates floor`): every fixture of the gate × the
 * three widths × the sample times × both languages, plus a world that
 * exercises every drawing path, give `__flOverlaps() == []`; and the check
 * itself catches the defects it exists for.
 */

import { assert, assertEqual, test } from '../test/harness';
import { CELL, LANE_BAND, metrics } from './constants';
import type { Box } from './draw';
import { checkGeometry } from './geometry';
import type { Cell, Line, Room } from './layout';
import { FLOOR_FIXTURES, SAMPLE_TIMES, T0, hardSnapshot, mountEngine, showcaseSnapshot } from './testing';

const WIDTHS = [1440, 1200, 900];
/** The canvas wrapper's width at a window width (16-px gutters; the sidebar is hidden on the Floor below 1200, ux-ia §2.8). */
const wrapWidth = (w: number) => w - 32;

test('T-floor-M16: the gate fixtures have no overlap at 1440/1200/900 × sample times × KO/EN', () => {
  let checked = 0;
  for (const fx of FLOOR_FIXTURES) {
    for (const width of WIDTHS) {
      for (const locale of ['ko', 'en'] as const) {
        for (const t of SAMPLE_TIMES) {
          const snap = showcaseSnapshot(fx.agents, fx.teams, t * 1000);
          const scope = fx.name === 'team-floor' && snap.teams[0] ? { kind: 'team' as const, teamKey: snap.teams[0].key } : { kind: 'global' as const };
          const m = mountEngine(snap, { width: wrapWidth(width), scope, locale });
          try {
            m.engine.freeze(T0 + t * 1000);
            const out = m.engine.overlaps();
            assertEqual(out.length, 0, `${fx.name} ${width} ${locale} t=${t}: ${JSON.stringify(out.slice(0, 3))}`);
            assert((m.engine.last?.cells.size ?? 0) > 0, `${fx.name}: seats are drawn`);
            checked++;
          } finally {
            m.unmount();
          }
        }
      }
    }
  }
  assertEqual(checked, FLOOR_FIXTURES.length * WIDTHS.length * 2 * SAMPLE_TIMES.length, 'every combination ran');
});

test('T-floor-M16: two teams in one bay, bottlenecks, mail in flight, busy traffic, a gone member, low quota', () => {
  for (const width of [1440, 1200, 900, 700, 480]) {
    for (const locale of ['ko', 'en'] as const) {
      for (const zoom of [0, 1, 2]) {
        const m = mountEngine(hardSnapshot(), { width: wrapWidth(width), locale });
        try {
          m.engine.zoomBy(zoom);
          m.engine.freeze(T0 + 15_000);
          const out = m.engine.overlaps();
          assertEqual(out.length, 0, `hard ${width} ${locale} zoom ${zoom}: ${JSON.stringify(out.slice(0, 3))}`);
          const f = m.engine.last;
          assert(f, 'a frame was drawn');
          assert(f.rooms.some((r) => r.lines.length >= 2), 'one bay holds two team lines');
        } finally {
          m.unmount();
        }
      }
    }
  }
});

test('T-floor-M16: the hard world draws what it is meant to (frames, envelopes, routes, batteries, sign rows)', () => {
  const m = mountEngine(hardSnapshot(), { width: 1408 });
  try {
    m.engine.freeze(T0 + 15_000);
    const boxes = m.engine.draw(true) as Box[];
    const has = (pred: (b: Box) => boolean) => boxes.some(pred);
    assert(has((b) => b.k === 'frame' && b.tag === 'bottleneck'), 'a bottleneck frame');
    assert(has((b) => b.k === 'badge' && b.tag === 'mail'), 'an envelope');
    assert(has((b) => b.k === 'badge' && b.tag === 'battery'), 'a battery');
    assert(has((b) => b.k === 'seg' && b.tag === 'traffic-lane'), 'a hand-off route');
    assert(has((b) => b.k === 'seg' && b.tag === 'dep'), 'a dependency line');
    assert(has((b) => b.k === 'label' && b.tag === 'sign'), 'a second team line has its sign');
    assert(has((b) => b.k === 'pane'), 'a glass pane');
    const lanes = boxes.filter((b) => b.k === 'seg' && b.tag === 'traffic-lane');
    for (const b of lanes) assert(b.line && b.y >= b.line.y && b.y < b.line.y + LANE_BAND, 'routes run in the lane band');
  } finally {
    m.unmount();
  }
});

// -- the check itself ---------------------------------------------------------

function fakeRoom(): Room {
  const r: Room = { key: 'g:p', projectId: null, label: 'p', x: 0, y: 0, w: 300, h: 200, lines: [], seats: [], cols: 3, rows: 1 };
  return r;
}

test('T-floor-M16 check: overlapping nameplates, a label on a desk and text outside its bay are caught', () => {
  const r = fakeRoom();
  const cells = new Map<string, Cell>([
    ['a', { id: 'a', x: 10, y: 40, room: r, line: null }],
    ['b', { id: 'b', x: 54, y: 40, room: r, line: null }],
  ]);
  const plate = (id: string, x: number, tag: string, y: number): Box => ({ k: 'label', x, y, w: 30, h: 7, id, room: r, zone: 'floor', tag });
  const ok: Box[] = [plate('a', 12, 'nameplate1', 73), plate('a', 12, 'nameplate2', 80), plate('b', 56, 'nameplate1', 73), plate('b', 56, 'nameplate2', 80)];
  assertEqual(checkGeometry(ok, cells, [r]).length, 0, 'a clean pair passes');
  const wide = [...ok.slice(0, 2), plate('b', 30, 'nameplate1', 73), plate('b', 56, 'nameplate2', 80)];
  const out = checkGeometry(wide, cells, [r]);
  assert(out.some((o) => o.kind === 'overlap'), 'two nameplates touching is an overlap');
  assert(out.some((o) => o.kind === 'nameplate'), 'a nameplate wider than its cell is caught');
  const onDesk = checkGeometry([...ok, { k: 'desk', x: 15, y: 75, w: 26, h: 12, id: 'a', room: r }], cells, [r]);
  assert(onDesk.some((o) => o.kind === 'overlap'), 'a nameplate on a desk is caught');
  const outside = checkGeometry([...ok, { k: 'label', x: 290, y: 100, w: 30, h: 7, room: r, zone: 'floor' }], cells, [r]);
  assert(outside.some((o) => o.kind === 'outside'), 'text past the bay edge is caught');
  const one = checkGeometry(ok.slice(1), cells, [r]);
  assert(one.some((o) => o.kind === 'nameplate'), 'a one-line nameplate is caught');
});

test('T-floor-M16 check: a missing or misplaced rail stop and a route outside the lane band are caught', () => {
  const r = fakeRoom();
  const team = { key: 't', line: ['a', 'b'], inferred: true } as unknown as Line['team'];
  const line: Line = { team, idx: 0, y: 24, room: r };
  r.lines = [line];
  const cells = new Map<string, Cell>([
    ['a', { id: 'a', x: 6, y: 48, room: r, line }],
    ['b', { id: 'b', x: 50, y: 48, room: r, line }],
  ]);
  const M = metrics(2);
  const plates: Box[] = ['a', 'b'].flatMap((id, i) => [
    { k: 'label', x: 8 + i * CELL, y: 81, w: 30, h: M.t1, id, room: r, zone: 'floor', tag: 'nameplate1' } as Box,
    { k: 'label', x: 8 + i * CELL, y: 81 + M.t1, w: 30, h: M.t2, id, room: r, zone: 'floor', tag: 'nameplate2' } as Box,
  ]);
  const stop = (x: number, id: string): Box => ({ k: 'stop', x: x - 1, y: 100, w: 2, h: 4, line, room: r, id });
  const good = [...plates, stop(28, 'a'), stop(72, 'b'), stop(116, 'done:t')];
  assertEqual(checkGeometry(good, cells, [r]).length, 0, 'one stop per station passes');
  assert(checkGeometry([...plates, stop(28, 'a'), stop(116, 'done:t')], cells, [r]).some((o) => o.kind === 'rail'), 'a desk without a stop is caught');
  assert(checkGeometry([...plates, stop(30, 'a'), stop(72, 'b'), stop(116, 'done:t')], cells, [r]).some((o) => o.kind === 'rail'), 'a stop off its desk centre is caught');
  const route: Box = { k: 'seg', x: 20, y: 40, w: 50, h: 1, line, room: r, tag: 'traffic-lane' };
  assert(checkGeometry([...good, route], cells, [r]).some((o) => o.kind === 'route'), 'a hand-off run below the lane band is caught');
  const fine: Box = { ...route, y: 26 };
  assertEqual(checkGeometry([...good, fine], cells, [r]).length, 0, 'a run inside the lane band passes');
  const sub: Box = { k: 'badge', x: 10.5, y: 60, w: 5, h: 4, id: 'a' };
  assert(checkGeometry([...good, sub], cells, [r]).some((o) => o.kind === 'subpixel'), 'art off the pixel grid is caught');
});

test('T-floor-M16: __flOverlaps reports a missing Floor instead of passing', () => {
  const m = mountEngine(showcaseSnapshot(8, ['native'], 0));
  m.unmount();
  const out = m.engine.overlaps();
  assertEqual(out.length, 1, 'one finding');
  assertEqual(out[0]?.kind, 'missing', 'the canvas is missing');
});
