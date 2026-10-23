/**
 * The showcase's scene registry, read from its source (`src/showcase/scenes.ts`,
 * `SCENE_NAMES`) so the browser gates cover every scene the showcase has — a
 * scene added there is gated without touching the gate (T31). The TypeScript
 * record `SCENES` is keyed by the same names (the compiler holds them equal).
 */

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

/** The scene names, in registry order. Throws when the registry cannot be read. */
export function parseSceneNames(source) {
  const m = /export const SCENE_NAMES = \[([\s\S]*?)\]\s*as const;/.exec(source);
  if (!m) throw new Error('SCENE_NAMES not found in src/showcase/scenes.ts');
  const names = [...m[1].matchAll(/'([a-z0-9][a-z0-9-]*)'/g)].map((x) => x[1]);
  if (names.length === 0) throw new Error('SCENE_NAMES is empty');
  if (new Set(names).size !== names.length) throw new Error('SCENE_NAMES has a duplicate');
  return names;
}

export function showcaseScenes(root) {
  return parseSceneNames(readFileSync(join(root, 'src', 'showcase', 'scenes.ts'), 'utf8'));
}
