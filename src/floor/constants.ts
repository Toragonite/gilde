/**
 * Floor geometry (pixel-office.md §3, prototype section 12 after floor-qa, M16).
 *
 * World units are art pixels. A station cell is CELL wide: the 32-px seat art
 * (chair, character, 26-px desk) centred in it, so a two-line nameplate of
 * about twelve characters fits at 2×. Text is drawn in screen space at a fixed
 * size, so the vertical bands that hold text are sized per scale (`metrics`).
 *
 * A team line, top to bottom: the traffic lane band (3 lanes, 3 px apart,
 * under the wall line or the line's sign) · the bubble band · the seat art
 * (chair, character, desk; shelves stand level with the desks) · a two-line
 * nameplate (glyph + name / state word) · the dependency lane · the rail, with
 * one stop per shelf and per desk · the station labels, one under each stop.
 * An open-grid row is the bubble band, the art and the nameplate.
 */

export const CELL = 44;
export const ART = 32;
export const AX = (CELL - ART) / 2;
export const WALL = 24;
export const PAD = 6;
export const MARGIN = 6;
export const SHELF_W = 34;
export const SHELF_H = 35;
export const SHELF_GAP = 4;
export const LANES = 3;
export const LANE_GAP = 3;
export const LANE_BAND = 10;
export const BUB_BAND = 14;
/** Nameplate top below the art origin (the desk legs end at 30). */
export const LAB = 33;
/** The flow strip's window, seconds. */
export const STRIP_WIN_S = 600;
/** Strip lanes besides "you". */
export const STRIP_LANES = 5;
/** Ended / stopped seats stay as ghosts this long (I3). */
export const GHOST_GRACE_MS = 10 * 60_000;
/** Recent traffic window (pixel-office §2.5). */
export const TRAFFIC_MS = 5 * 60_000;
/** At most this many planes in flight per bay (storm control, §2.5). */
export const PLANES_PER_BAY = 6;
/** Fitted scales, largest first; zoom may go up to MAX_SCALE. */
export const FIT_SCALES = [4, 3, 2] as const;
export const MIN_SCALE = 2;
export const MAX_SCALE = 6;
/** Cap on the canvas backing store, device pixels (memory guard). */
export const MAX_CANVAS_PIXELS = 16_000_000;
/** The minimum CSS width the Floor lays out for. */
export const MIN_CSS_W = 240;

export interface Metrics {
  /** Scale (CSS px per world px). */
  s: number;
  /** Height of the 11-px name line, world px. */
  t1: number;
  /** Height of a 10-px line, world px. */
  t2: number;
  /** Nameplate bottom below the art origin. */
  lb: number;
  /** One team line: lane band + bubble band + art + nameplate + dependency lane + rail + labels. */
  lineH: number;
  /** One open-grid row. */
  openH: number;
  /** Strip lane pitch. */
  pitch: number;
  /** The sign row above a second (third …) team line of one bay. */
  signH: number;
}

export function metrics(s: number): Metrics {
  const t1 = Math.ceil(14 / s);
  const t2 = Math.ceil(13 / s);
  const lb = LAB + t1 + t2;
  return {
    s,
    t1,
    t2,
    lb,
    lineH: LANE_BAND + BUB_BAND + lb + 12 + t2,
    openH: BUB_BAND + lb + 2,
    pitch: Math.max(5, Math.ceil(12 / s)),
    signH: Math.ceil(15 / s) + 4,
  };
}
