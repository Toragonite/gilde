/**
 * Original pixel art, generated in code (pixel-office G7): 12 × 16 character
 * templates, the vendor accessories (shape only, one neutral grey), poses,
 * 5-px glyphs and the bubble. No asset files, no third-party sprites, no
 * vendor logos.
 *
 * Template letters: h hair · s skin · e eye · t shirt · u shirt shade ·
 * p pants · o shoes · a / A accessory.
 */

import type { Vendor } from '../lib/gen/Vendor';
import type { Palette } from './palette';

/** The subset of CanvasRenderingContext2D the Floor draws with (tests pass a recorder). */
export interface Ctx2D {
  fillStyle: string | CanvasGradient | CanvasPattern;
  globalAlpha: number;
  font: string;
  textAlign: CanvasTextAlign;
  textBaseline: CanvasTextBaseline;
  imageSmoothingEnabled: boolean;
  fillRect(x: number, y: number, w: number, h: number): void;
  fillText(text: string, x: number, y: number): void;
  measureText(text: string): { width: number };
  setTransform(a: number, b: number, c: number, d: number, e: number, f: number): void;
  drawImage(image: CanvasImageSource, dx: number, dy: number): void;
}

const SPR_BASE = [
  '....hhhh....', '...hhhhhh...', '..hhhhhhhh..', '..hsessesh..', '..ssessess..', '..ssssssss..', '...ssssss...', '..tttttttt..',
  '.uttttttttu.', '.uttttttttu.', '.stttttttts.', '..tttttttt..', '..pppppppp..', '..ppp..ppp..', '..ppp..ppp..', '..ooo..ooo..',
];
const SPR_RAISE = [
  '....hhhh..ss', '...hhhhhh.ss', '..hhhhhhhh.t', '..hsessesh.t', '..ssessess.t', '..ssssssss.t', '...ssssss..t', '..tttttttttt',
  '.utttttttt..', '.utttttttt..', '.stttttttt..', '..tttttttt..', '..pppppppp..', '..ppp..ppp..', '..ppp..ppp..', '..ooo..ooo..',
];
const SPR_WALK = [
  ['..ppp..ppp..', '..pp....pp..', '.oo......oo.'],
  ['...pppppp...', '....pppp....', '....oooo....'],
];

export type Pose = 'sit' | 'raise' | 'tilt' | 'slump' | 'faceplant' | 'lean' | 'walk0' | 'walk1' | 'stand';

function accPixels(vendor: Vendor): [number, number, string][] {
  const p: [number, number, string][] = [];
  const row = (y: number, x0: number, x1: number, c: string) => {
    for (let x = x0; x <= x1; x++) p.push([x, y, c]);
  };
  if (vendor === 'claude') {
    // hood arch
    row(0, 3, 8, 'a');
    p.push([2, 1, 'a'], [9, 1, 'a']);
    for (let y = 2; y <= 7; y++) p.push([1, y, 'a'], [10, y, 'a']);
  } else if (vendor === 'codex') {
    // cap with a side visor
    row(0, 3, 8, 'a');
    row(1, 2, 9, 'a');
    row(2, 2, 11, 'A');
  } else if (vendor === 'cursor') {
    // over-ear headphones
    row(0, 3, 8, 'a');
    p.push([2, 1, 'a'], [9, 1, 'a']);
    for (let y = 3; y <= 5; y++) p.push([1, y, 'a'], [2, y, 'a'], [9, y, 'a'], [10, y, 'a']);
  } else if (vendor === 'agy') {
    // beanie with a pom
    p.push([5, 0, 'A'], [6, 0, 'A']);
    row(1, 3, 8, 'A');
    row(2, 2, 9, 'a');
  }
  return p;
}

function moveHead(g: string[][], dx: number, dy: number): string[][] {
  const out = g.map((r) => r.slice());
  for (let y = 0; y < 7; y++) for (let x = 0; x < 12; x++) (out[y] as string[])[x] = '.';
  for (let y = 0; y < 7; y++) {
    for (let x = 0; x < 12; x++) {
      const c = (g[y] as string[])[x] as string;
      if (c === '.') continue;
      const nx = x + dx;
      const ny = y + dy;
      if (nx >= 0 && nx < 12 && ny >= 0 && ny < 16) (out[ny] as string[])[nx] = c;
    }
  }
  return out;
}

/** The 12 × 16 grid of a pose for a vendor. */
export function poseGrid(pose: Pose, vendor: Vendor): string[][] {
  let g = (pose === 'raise' ? SPR_RAISE : SPR_BASE).map((r) => r.split(''));
  for (const [x, y, c] of accPixels(vendor)) (g[y] as string[])[x] = c;
  const at = (y: number, x: number) => (g[y] as string[])[x];
  const put = (y: number, x: number, c: string) => {
    (g[y] as string[])[x] = c;
  };
  if (pose === 'tilt') g = moveHead(g, 1, 0);
  if (pose === 'slump') {
    g = moveHead(g, 0, 1);
    for (let y = 0; y < 15; y++) for (let x = 0; x < 12; x++) if (at(y, x) === 'e' && at(y + 1, x) === 'e') put(y, x, 's');
  }
  if (pose === 'faceplant') {
    g = moveHead(g, 0, 3);
    for (let y = 0; y < 16; y++) for (let x = 0; x < 12; x++) if (at(y, x) === 'e') put(y, x, 'h');
  }
  if (pose === 'lean') {
    put(2, 1, 's');
    put(3, 1, 's');
    put(2, 10, 's');
    put(3, 10, 's');
    for (let y = 4; y <= 7; y++) {
      put(y, 1, 'u');
      put(y, 10, 'u');
    }
    for (let y = 8; y <= 10; y++) {
      put(y, 1, '.');
      put(y, 10, '.');
    }
  }
  if (pose === 'walk0' || pose === 'walk1') {
    const legs = SPR_WALK[pose === 'walk0' ? 0 : 1] as string[];
    for (let i = 0; i < 3; i++) g[13 + i] = (legs[i] as string).split('');
  }
  return g;
}

/** FNV-1a, for the stable per-session skin/hair/shirt choice. */
export function flHash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

/** A `#rrggbb` token value darkened by `f`; anything else is returned as is. */
export function shade(hex: string, f: number): string {
  if (!/^#[0-9a-f]{6}$/i.test(hex)) return hex;
  const n = parseInt(hex.slice(1), 16);
  const ch = (v: number) => Math.max(0, Math.min(255, Math.round(v * f)));
  return '#' + ((1 << 24) | (ch(n >> 16) << 16) | (ch((n >> 8) & 255) << 8) | ch(n & 255)).toString(16).slice(1);
}

export interface Identity {
  vendor: Vendor;
  skin: string;
  hair: string;
  shirt: string;
}

/** Skin and hair from the seat key's hash; the shirt is the identity colour [M12] or a neutral tone. */
export function identityOf(P: Palette, key: string, vendor: Vendor, colour: string | null, hairShift = 0): Identity {
  const h = flHash(key);
  const pick = (list: string[], i: number) => list[i % Math.max(1, list.length)] ?? '';
  return {
    vendor,
    skin: pick(P.skins, h % 5),
    hair: pick(P.hairs, ((h >>> 3) + hairShift) % 6),
    shirt: colour ?? pick(P.shirts, (h >>> 7) % 4),
  };
}

export type CanvasFactory = (w: number, h: number) => HTMLCanvasElement | null;

export const defaultCanvasFactory: CanvasFactory = (w, h) => {
  if (typeof document === 'undefined') return null;
  const c = document.createElement('canvas');
  c.width = w;
  c.height = h;
  return c;
};

/** Composed sprites, one 14 × 18 canvas per identity × pose (the 1-px outline included). */
export class SpriteCache {
  private readonly cache = new Map<string, HTMLCanvasElement | null>();
  private sig = '';

  constructor(private readonly make: CanvasFactory = defaultCanvasFactory) {}

  /** Drop everything when the palette changed (theme switch). */
  sync(signature: string): void {
    if (signature === this.sig) return;
    this.sig = signature;
    this.cache.clear();
  }

  get size(): number {
    return this.cache.size;
  }

  get(P: Palette, id: Identity, pose: Pose, ghost: boolean): HTMLCanvasElement | null {
    const k = [id.vendor, id.skin, id.hair, id.shirt, pose, ghost ? 1 : 0].join('|');
    if (this.cache.has(k)) return this.cache.get(k) ?? null;
    const c = this.make(14, 18);
    const x = c ? c.getContext('2d') : null;
    if (!c || !x) {
      this.cache.set(k, null);
      return null;
    }
    const g = poseGrid(pose, id.vendor);
    const cmap: Record<string, string> = { h: id.hair, s: id.skin, e: P.eye, t: id.shirt, u: shade(id.shirt, 0.72), p: P.pants, o: P.shoes, a: P.acc, A: P.accLight };
    const op = (xx: number, yy: number) => yy >= 0 && yy < 16 && xx >= 0 && xx < 12 && (g[yy] as string[])[xx] !== '.';
    const keep = (xx: number, yy: number) => !ghost || ((xx + yy) & 1) === 0;
    x.fillStyle = P.outline;
    for (let yy = -1; yy <= 16; yy++) {
      for (let xx = -1; xx <= 12; xx++) {
        if (op(xx, yy)) continue;
        if ((op(xx - 1, yy) || op(xx + 1, yy) || op(xx, yy - 1) || op(xx, yy + 1)) && keep(xx, yy)) x.fillRect(xx + 1, yy + 1, 1, 1);
      }
    }
    for (let yy = 0; yy < 16; yy++) {
      for (let xx = 0; xx < 12; xx++) {
        const ch = (g[yy] as string[])[xx] as string;
        if (ch === '.' || !keep(xx, yy)) continue;
        x.fillStyle = cmap[ch] ?? P.outline;
        x.fillRect(xx + 1, yy + 1, 1, 1);
      }
    }
    this.cache.set(k, c);
    return c;
  }
}

export const GLYPH: Readonly<Record<string, readonly string[]>> = {
  cross: ['x...x', '.x.x.', '..x..', '.x.x.', 'x...x'],
  quest: ['.xxx.', '...x.', '..xx.', '.....', '..x..'],
  env: ['x...x', '.x.x.', '..x..', '.....'],
  lock: ['.x.', 'x.x', 'xxx', 'xxx'],
  arrow: ['xxx', '.xx', 'x.x'],
  tri: ['..x..', '..x..', '.xxx.', '.xxx.', 'xxxxx'],
  down: ['..x..', '..x..', 'xxxxx', '.xxx.', '..x..'],
  wave: ['.....', '.x...', 'x.x.x', '...x.', '.....'],
  dot: ['.....', '.xxx.', '.xxx.', '.xxx.', '.....'],
};

export function px(c: Ctx2D, x: number, y: number, w: number, h: number, col: string): void {
  c.fillStyle = col;
  c.fillRect(x, y, w, h);
}

export function drawGlyph(c: Ctx2D, name: keyof typeof GLYPH, x: number, y: number, col: string): void {
  const g = GLYPH[name] ?? [];
  c.fillStyle = col;
  for (let yy = 0; yy < g.length; yy++) {
    const row = g[yy] as string;
    for (let xx = 0; xx < row.length; xx++) if (row[xx] === 'x') c.fillRect(x + xx, y + yy, 1, 1);
  }
}

export interface BubbleOptions {
  dashed?: boolean;
  heavy?: boolean;
  /** The ↗ "answer at its owner" mark (a Watching session's approval or question). */
  corner?: boolean;
}

/** A 9 × 8 bordered bubble with a tail; glyph colour = `--px-glyph`, border = a state token. */
export function drawBubble(c: Ctx2D, P: Palette, x: number, y: number, glyph: keyof typeof GLYPH | null, border: string, opt: BubbleOptions = {}): void {
  px(c, x + 1, y + 1, 7, 6, P.bubble);
  const edge: [number, number][] = [];
  for (let i = 1; i <= 7; i++) {
    edge.push([x + i, y]);
    edge.push([x + i, y + 7]);
  }
  for (let i = 1; i <= 6; i++) {
    edge.push([x, y + i]);
    edge.push([x + 8, y + i]);
  }
  c.fillStyle = border;
  edge.forEach(([ex, ey], i) => {
    if (!opt.dashed || i % 2 === 0) c.fillRect(ex, ey, 1, 1);
  });
  if (opt.heavy) {
    px(c, x + 1, y - 1, 7, 1, border);
    px(c, x - 1, y + 1, 1, 6, border);
    px(c, x + 9, y + 1, 1, 6, border);
    px(c, x + 1, y + 8, 7, 1, border);
  }
  px(c, x + 2, y + 7, 1, 1, P.bubble);
  px(c, x + 1, y + 8, 1, 1, border);
  px(c, x, y + 9, 1, 1, border);
  if (glyph) drawGlyph(c, glyph, x + 2, y + 1, P.glyph);
  if (opt.corner) drawGlyph(c, 'arrow', x + 10, y, border);
}
