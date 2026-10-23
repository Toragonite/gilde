/**
 * Which backend answers the IPC boundary.
 *
 * `VITE_GILDE_SHOWCASE=1` builds the showcase: the real UI on demo data in a
 * plain browser with no Tauri runtime. The demo code (`src/showcase/`) is
 * loaded only through a dynamic `import()` guarded by this constant, so a
 * normal build drops it (`scripts/check-showcase-tree-shaking.mjs`). While it
 * is on, `commands.ts` and `events.ts` never reach Tauri.
 */

import type { ChannelLike } from './channel';
import type { CommandName } from './gen/commands';

/**
 * What answers commands in showcase mode: the demo backend of `src/showcase/`,
 * installed with `installShowcaseBackend` (lib/commands.ts) by the showcase
 * itself. `channel` is set for the streaming commands. Binding for T19 (A8.3).
 */
export type ShowcaseBackend = (
  name: CommandName,
  req: unknown,
  channel: ChannelLike<unknown> | undefined,
) => Promise<unknown>;

export const SHOWCASE: boolean = import.meta.env.VITE_GILDE_SHOWCASE === '1';

let override: boolean | null = null;

/** Whether the boundary answers from the showcase instead of Tauri. */
export function showcaseMode(): boolean {
  return override ?? SHOWCASE;
}

/** Tests only: force showcase mode on or off; `null` restores the build value. */
export function setShowcaseModeForTests(value: boolean | null): void {
  override = value;
}
