/**
 * Pure layout rules of Split and Team › Panes (ux-ia §3.7–§3.9, J4), kept out
 * of the components so the tests can pin them.
 *
 *  - Split: 1 tile → one; 2 → side by side; 3 → 1 + 2; 4 → 2×2; 5–6 → 3×2.
 *    At 900–1199 px at most two columns show (5–6 → 2×3); below 900 px one
 *    tile shows with a tile strip (the screen renders only the focused tile).
 *  - Team › Panes: the lead takes the left column (40 % by default, resizable),
 *    teammates tile on the right in two columns (one column at 900–1199 px);
 *    teammates that do not fit fold into the strip.
 *  - Spatial focus (`⌥⌘` arrows) works on the logical grid, never on pixels,
 *    so it is the same in every theme, zoom and test environment.
 *  - "Least recently focused" decides which tile a 7th session or a strip item
 *    replaces.
 */

import type { WidthBand } from '../shell/layout';

/** A tile's place in the logical grid (0-based). */
export interface Cell {
  col: number;
  row: number;
  colSpan: number;
  rowSpan: number;
}

export interface GridSpec {
  columns: number;
  rows: number;
  /** `cells[i]` places tile i. */
  cells: Cell[];
}

export const MAX_TILES = 6;

const cell = (col: number, row: number, colSpan = 1, rowSpan = 1): Cell => ({ col, row, colSpan, rowSpan });

/**
 * The grid of an n-tile Split (n is clamped to 0..6). Narrow windows show one
 * tile: the spec then has a single cell and the screen picks the tile.
 */
export function splitGrid(n: number, band: WidthBand): GridSpec {
  const count = Math.max(0, Math.min(MAX_TILES, Math.floor(n)));
  if (count === 0) return { columns: 1, rows: 1, cells: [] };
  if (band === 'narrow' || count === 1) return { columns: 1, rows: 1, cells: [cell(0, 0)] };
  if (count === 2) return { columns: 2, rows: 1, cells: [cell(0, 0), cell(1, 0)] };
  if (count === 3) return { columns: 2, rows: 2, cells: [cell(0, 0, 1, 2), cell(1, 0), cell(1, 1)] };
  if (count === 4) return { columns: 2, rows: 2, cells: [cell(0, 0), cell(1, 0), cell(0, 1), cell(1, 1)] };
  if (band === 'medium') {
    // At most two columns: 2×3, the fifth tile spans the last row.
    const cells = [cell(0, 0), cell(1, 0), cell(0, 1), cell(1, 1)];
    cells.push(count === 5 ? cell(0, 2, 2) : cell(0, 2), ...(count === 6 ? [cell(1, 2)] : []));
    return { columns: 2, rows: 3, cells };
  }
  // 3×2; with five tiles the last one spans two columns so no hole is left.
  const cells = [cell(0, 0), cell(1, 0), cell(2, 0), cell(0, 1)];
  cells.push(count === 5 ? cell(1, 1, 2) : cell(1, 1), ...(count === 6 ? [cell(2, 1)] : []));
  return { columns: 3, rows: 2, cells };
}

/** How many teammate tiles fit next to the lead (the rest fold into the strip). */
export function memberCapacity(band: WidthBand): number {
  if (band === 'wide') return 4;
  if (band === 'medium') return 3;
  return 0;
}

/**
 * Team › Panes: cell 0 is the lead (column 0, every row), cells 1..m the
 * visible teammates on the right. `m` is clamped to the band's capacity.
 * With no teammate the lead takes the whole area.
 */
export function panesGrid(members: number, band: WidthBand): GridSpec {
  const m = Math.max(0, Math.min(memberCapacity(band), Math.floor(members)));
  if (m === 0) return { columns: 1, rows: 1, cells: [cell(0, 0)] };
  if (band === 'medium') {
    const cells = [cell(0, 0, 1, m)];
    for (let i = 0; i < m; i++) cells.push(cell(1, i));
    return { columns: 2, rows: m, cells };
  }
  if (m === 1) return { columns: 2, rows: 1, cells: [cell(0, 0), cell(1, 0)] };
  if (m === 2) return { columns: 3, rows: 1, cells: [cell(0, 0), cell(1, 0), cell(2, 0)] };
  if (m === 3) return { columns: 3, rows: 2, cells: [cell(0, 0, 1, 2), cell(1, 0), cell(2, 0), cell(1, 1, 2)] };
  return { columns: 3, rows: 2, cells: [cell(0, 0, 1, 2), cell(1, 0), cell(2, 0), cell(1, 1), cell(2, 1)] };
}

/** The lead column's share of the width: default 40 %, kept within 20–80 % (A30.3). */
export const LEAD_RATIO_DEFAULT = 0.4;
export const LEAD_RATIO_MIN = 0.2;
export const LEAD_RATIO_MAX = 0.8;

export function clampRatio(r: number): number {
  if (!Number.isFinite(r)) return LEAD_RATIO_DEFAULT;
  return Math.round(Math.min(LEAD_RATIO_MAX, Math.max(LEAD_RATIO_MIN, r)) * 100) / 100;
}

/** `grid-column` / `grid-row` of a cell (CSS lines are 1-based). */
export function cellStyle(c: Cell, colOffset = 0): { gridColumn: string; gridRow: string } {
  return { gridColumn: `${c.col + 1 + colOffset} / span ${c.colSpan}`, gridRow: `${c.row + 1} / span ${c.rowSpan}` };
}

export type Direction = 'left' | 'right' | 'up' | 'down';

function overlap(a0: number, a1: number, b0: number, b1: number): number {
  return Math.max(0, Math.min(a1, b1) - Math.max(a0, b0));
}

/**
 * The tile `⌥⌘` + arrow moves to: the nearest cell beyond the current one in
 * that direction, preferring cells that share rows (or columns) with it, then
 * the closest centre, then reading order. `null` at the edge.
 */
export function spatialNext(cells: readonly Cell[], from: number, dir: Direction): number | null {
  const f = cells[from];
  if (!f) return null;
  let bestIndex: number | null = null;
  let bestKey: number[] = [];
  for (let index = 0; index < cells.length; index++) {
    const c = cells[index] as Cell;
    if (index === from) continue;
    let gap: number;
    let shared: number;
    let centre: number;
    if (dir === 'right' || dir === 'left') {
      gap = dir === 'right' ? c.col - (f.col + f.colSpan) : f.col - (c.col + c.colSpan);
      shared = overlap(f.row, f.row + f.rowSpan, c.row, c.row + c.rowSpan);
      centre = Math.abs(c.row + c.rowSpan / 2 - (f.row + f.rowSpan / 2));
    } else {
      gap = dir === 'down' ? c.row - (f.row + f.rowSpan) : f.row - (c.row + c.rowSpan);
      shared = overlap(f.col, f.col + f.colSpan, c.col, c.col + c.colSpan);
      centre = Math.abs(c.col + c.colSpan / 2 - (f.col + f.colSpan / 2));
    }
    if (gap < 0) continue;
    const key = [gap, shared > 0 ? 0 : 1, centre, index];
    if (bestIndex === null || compare(key, bestKey) < 0) {
      bestIndex = index;
      bestKey = key;
    }
  }
  return bestIndex;
}

function compare(a: readonly number[], b: readonly number[]): number {
  for (let i = 0; i < a.length; i++) {
    const d = (a[i] as number) - (b[i] as number);
    if (d !== 0) return d;
  }
  return 0;
}

/** Move `key` to the front of a most-recently-focused list (bounded). */
export function touch<K>(order: readonly K[], key: K, max = 64): K[] {
  return [key, ...order.filter((k) => k !== key)].slice(0, max);
}

/**
 * The least recently focused of `candidates`: one never focused comes first
 * (the earliest such in candidate order), else the one focused longest ago.
 */
export function leastRecent<K>(order: readonly K[], candidates: readonly K[]): K | null {
  if (candidates.length === 0) return null;
  const never = candidates.find((c) => !order.includes(c));
  if (never !== undefined) return never;
  let pick = candidates[0] as K;
  let at = -1;
  for (const c of candidates) {
    const i = order.indexOf(c);
    if (i > at) {
      at = i;
      pick = c;
    }
  }
  return pick;
}
