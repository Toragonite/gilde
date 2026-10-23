/**
 * The Floor's colours, read from CSS custom properties (never literals: every
 * colour lives in `styles/tokens.css`).
 *
 * `floor.css` defines one `--fl-<name>` per drawing colour as
 * `var(--px-<name>, <an existing neutral or semantic token>)`, per theme. The
 * `--px-*` pixel palette and the eight `--id-*` identity tokens [M12] are the
 * Floor's own tokens in `tokens.css`; until they exist the Floor draws with
 * the neutral scale, which keeps every state legible (state is never colour
 * alone, pixel-office G6).
 */

import type { IdentityColour } from '../lib/gen/IdentityColour';

/** Drawing colours (camelCase of the `--px-*` names). */
export const PX_KEYS = [
  'bg', 'floor-a', 'floor-b', 'wall', 'wall-top', 'trim', 'door', 'door-dark', 'knob', 'desk', 'desk-top', 'desk-edge',
  'chair', 'chair-dark', 'monitor', 'screen-on', 'screen-line', 'screen-off', 'glass', 'glint', 'board', 'board-edge',
  'board-line', 'paper', 'paper-edge', 'ink', 'outline', 'pants', 'shoes', 'acc', 'acc-light', 'smoke', 'text',
  'text-strong', 'bubble', 'glyph', 'eye',
] as const;

export type PxKey = (typeof PX_KEYS)[number];

type Camel<S extends string> = S extends `${infer A}-${infer B}` ? `${A}${Capitalize<Camel<B>>}` : S;

export type PxName = Camel<PxKey>;

export const IDENTITY_COLOURS: readonly IdentityColour[] = ['red', 'blue', 'green', 'yellow', 'purple', 'orange', 'pink', 'cyan'];

export type Palette = Record<PxName, string> & {
  ok: string;
  warn: string;
  error: string;
  info: string;
  accent: string;
  sans: string;
  id: Record<IdentityColour, string>;
  shirts: string[];
  skins: string[];
  hairs: string[];
};

export function camel(key: string): string {
  return key.replace(/-([a-z])/g, (_m, c: string) => c.toUpperCase());
}

/** Read the palette from an element inside the Floor root (`.fl-root` defines the `--fl-*` set). */
export function readPalette(el: Element | null): Palette {
  const get = (() => {
    try {
      if (el && typeof getComputedStyle === 'function') {
        const cs = getComputedStyle(el);
        return (name: string) => cs.getPropertyValue(name).trim();
      }
    } catch {
      // No style engine (tests): the empty palette below.
    }
    return () => '';
  })();
  const P = {} as Record<string, unknown>;
  for (const k of PX_KEYS) P[camel(k)] = get(`--fl-${k}`);
  for (const k of ['ok', 'warn', 'error', 'info', 'accent']) P[k] = get(`--fl-${k}`) || get(`--${k}`);
  P.sans = get('--sans') || 'system-ui, sans-serif';
  const id = {} as Record<IdentityColour, string>;
  for (const c of IDENTITY_COLOURS) id[c] = get(`--fl-id-${c}`);
  P.id = id;
  P.shirts = [1, 2, 3, 4].map((i) => get(`--fl-shirt-${i}`));
  P.skins = [1, 2, 3, 4, 5].map((i) => get(`--fl-skin-${i}`));
  P.hairs = [1, 2, 3, 4, 5, 6].map((i) => get(`--fl-hair-${i}`));
  return P as Palette;
}

/** A signature that changes when the theme (or the tokens) change: the sprite cache key. */
export function paletteSignature(P: Palette): string {
  return [P.bg, P.text, P.paper, P.outline, P.skins.join(','), P.id.red, P.shirts[0]].join('|');
}
