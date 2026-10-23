/**
 * The commands the WebView may call (contract §4.1–§4.2).
 *
 * Every function of `gen/commands.ts` (generated from the manifest) is wrapped
 * once, here, so that:
 *  - every rejection is an `IpcError` — a platform string or a thrown
 *    `TypeError` becomes `INTERNAL` with a fresh correlation id (A7.3,
 *    `ipcError.ts`);
 *  - in showcase mode the call goes to the installed showcase backend instead
 *    of Tauri (`mode.ts`).
 * Screens call `commands.session_start(req)` etc.; nothing else in `src/`
 * calls `invoke`.
 */

import * as gen from './gen/commands';
import { COMMAND_NAMES } from './gen/commands';
import type { CommandName } from './gen/commands';
import type { ChannelLike } from './channel';
import type { IpcError } from './gen/IpcError';
import { toIpcError } from './ipcError';
import { showcaseMode } from './mode';
import type { ShowcaseBackend } from './mode';

export type { CommandName };
export { COMMAND_NAMES };

type Generated = typeof gen;

/** One wrapper per manifest command, with the generated signature. */
export type Commands = { readonly [K in CommandName]: Generated[K] };

// ---------------------------------------------------------------------------
// Showcase boundary
// ---------------------------------------------------------------------------

// The showcase backend type lives in mode.ts (A8.3); the showcase itself is
// never imported from here, so a normal build does not carry it.
export type { ShowcaseBackend };

let showcaseBackend: ShowcaseBackend | null = null;

/** Install (or, with `null`, remove) the showcase backend. Only `src/showcase/` and tests call this. */
export function installShowcaseBackend(backend: ShowcaseBackend | null): void {
  showcaseBackend = backend;
}

/** A command asked in showcase mode before a backend is installed. */
function showcaseRefusal(): IpcError {
  return { code: 'UNSUPPORTED', key: 'error.unsupported', params: { cap: 'showcase' }, detail: null, retryable: false };
}

// ---------------------------------------------------------------------------
// The wrappers
// ---------------------------------------------------------------------------

function wrap<K extends CommandName>(name: K): Generated[K] {
  const generated = gen[name] as unknown as (...args: unknown[]) => Promise<unknown>;
  const wrapped = (...args: unknown[]): Promise<unknown> => {
    let pending: Promise<unknown>;
    try {
      if (showcaseMode()) {
        pending = showcaseBackend
          ? showcaseBackend(name, args[0], args[1] as ChannelLike<unknown> | undefined)
          : Promise.reject(showcaseRefusal());
      } else {
        pending = generated(...args);
      }
    } catch (thrown) {
      pending = Promise.reject(thrown);
    }
    return pending.then(undefined, (reason: unknown) => {
      throw toIpcError(reason, name);
    });
  };
  return wrapped as unknown as Generated[K];
}

export const commands: Commands = Object.freeze(
  Object.fromEntries(COMMAND_NAMES.map((name) => [name, wrap(name)])),
) as Commands;
