/**
 * A fake core for tests: turns on showcase mode and installs a backend that
 * answers commands from per-command handlers and records every call. Screens'
 * tests use it to drive the real `commands` wrappers without Tauri.
 *
 *   await withFakeCore({ app_bootstrap: () => bootstrap }, async (core) => {
 *     ...
 *     assertEqual(core.calls('ui_state_set').length, 1, '…');
 *   });
 */

import type { ChannelLike } from '../lib/channel';
import { installShowcaseBackend } from '../lib/commands';
import type { CommandName } from '../lib/commands';
import { setShowcaseModeForTests } from '../lib/mode';

export type Handler = (req: unknown, channel: ChannelLike<unknown> | undefined) => unknown;

export interface FakeCall {
  name: CommandName;
  req: unknown;
  channel: ChannelLike<unknown> | undefined;
}

export interface FakeCore {
  /** Every call so far, in order. */
  readonly log: FakeCall[];
  calls(name: CommandName): FakeCall[];
  /** Replace or add a handler while the test runs. */
  on(name: CommandName, handler: Handler): void;
}

/**
 * Handlers return a value (resolved), a Promise, or throw (rejected with the
 * thrown value). A command without a handler rejects with `UNSUPPORTED{notBuilt}`,
 * like a wave-1 stub.
 */
export function fakeCore(handlers: Partial<Record<CommandName, Handler>> = {}): FakeCore {
  const table = new Map<CommandName, Handler>(Object.entries(handlers) as [CommandName, Handler][]);
  const log: FakeCall[] = [];
  installShowcaseBackend(async (name, req, channel) => {
    log.push({ name, req, channel });
    const handler = table.get(name);
    if (!handler) {
      throw { code: 'UNSUPPORTED', key: 'error.unsupported', params: { cap: 'notBuilt' }, detail: null, retryable: false };
    }
    return handler(req, channel);
  });
  setShowcaseModeForTests(true);
  return {
    log,
    calls: (name) => log.filter((c) => c.name === name),
    on: (name, handler) => {
      table.set(name, handler);
    },
  };
}

export function resetFakeCore(): void {
  installShowcaseBackend(null);
  setShowcaseModeForTests(null);
}

/** Run `body` against a fake core, restoring the real boundary afterwards. */
export async function withFakeCore<T>(
  handlers: Partial<Record<CommandName, Handler>>,
  body: (core: FakeCore) => T | Promise<T>,
): Promise<T> {
  const core = fakeCore(handlers);
  try {
    return await body(core);
  } finally {
    resetFakeCore();
  }
}

/** Let pending promise callbacks run (a few microtask turns). */
export async function flush(turns = 5): Promise<void> {
  for (let i = 0; i < turns; i++) await Promise.resolve();
}
