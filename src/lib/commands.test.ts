/**
 * The command wrappers (contract §4.1, §4.4, A7.3).
 *
 * T-ipc-platformReject: a rejection that is not an `IpcError` — Tauri's plain
 * string for malformed JSON / lone surrogates / depth / an ungranted command,
 * or a thrown TypeError — becomes `INTERNAL` with a fresh correlation id; the
 * platform string is never in the error and never logged (only "platform
 * rejection", the command name and the length).
 */

import { clearMocks, mockIPC } from '@tauri-apps/api/mocks';

import { assert, assertDeepEqual, assertEqual, assertMatch, assertRejects, test } from '../test/harness';
import { fakeCore, resetFakeCore } from '../test/backend';
import { COMMAND_NAMES, commands, installShowcaseBackend } from './commands';
import * as gen from './gen/commands';
import type { IpcError } from './gen/IpcError';
import { isIpcError, setRejectionLogForTests, toIpcError } from './ipcError';
import type { PlatformRejectionLog } from './ipcError';
import { setShowcaseModeForTests } from './mode';

const SECRET = 'invalid args `req` for command `turn_send`: unknown field `sk-ant-canary`, expected `text`';

/** Capture console output and the rejection log for the duration of `body`. */
async function capture(body: () => Promise<void>): Promise<{ console: string; log: PlatformRejectionLog[] }> {
  const lines: string[] = [];
  const log: PlatformRejectionLog[] = [];
  const original = { log: console.log, warn: console.warn, error: console.error, info: console.info, debug: console.debug };
  const sink = (...args: unknown[]) => lines.push(args.map((a) => (typeof a === 'string' ? a : JSON.stringify(a))).join(' '));
  console.log = sink;
  console.warn = sink;
  console.error = sink;
  console.info = sink;
  console.debug = sink;
  setRejectionLogForTests((message, fields) => {
    log.push(fields);
    sink(message, fields);
  });
  try {
    await body();
  } finally {
    Object.assign(console, original);
    setRejectionLogForTests(null);
  }
  return { console: lines.join('\n'), log };
}

test('T-ipc-platformReject: a plain-string rejection becomes INTERNAL with a fresh correlation id', async () => {
  mockIPC(() => {
    throw SECRET;
  });
  try {
    const errors: IpcError[] = [];
    const out = await capture(async () => {
      errors.push((await assertRejects(() => commands.turn_send({} as never), 'turn_send rejects')) as IpcError);
      errors.push((await assertRejects(() => commands.app_mark({} as never), 'app_mark rejects')) as IpcError);
    });
    for (const e of errors) {
      assert(isIpcError(e), 'the rejection is an IpcError');
      assertEqual(e.code, 'INTERNAL', 'code INTERNAL');
      assertEqual(e.key, 'error.internal', 'key error.internal');
      assertDeepEqual(e.params, {}, 'no params');
      assertEqual(e.retryable, false, 'not retryable');
      assertMatch(e.detail ?? '', /^ui-[0-9a-f]{16}$/, 'detail is a correlation id');
      assert(!JSON.stringify(e).includes('sk-ant-canary'), 'the platform string is not in the error');
    }
    assert(errors[0]?.detail !== errors[1]?.detail, 'each rejection gets its own correlation id');
    assert(!out.console.includes('sk-ant-canary') && !out.console.includes('invalid args'), 'the platform string is never logged');
    assertEqual(out.log.length, 2, 'one log line per rejection');
    assertDeepEqual(
      out.log.map((l) => [l.command, l.kind, l.length]),
      [
        ['turn_send', 'string', SECRET.length],
        ['app_mark', 'string', SECRET.length],
      ],
      'the log carries only the command, the kind and the length',
    );
    assertEqual(out.log[0]?.correlationId, errors[0]?.detail, 'the log line names the same correlation id');
  } finally {
    clearMocks();
  }
});

test('T-ipc-platformReject: no Tauri runtime (a thrown TypeError) is INTERNAL too', async () => {
  clearMocks();
  const out = await capture(async () => {
    const e = (await assertRejects(() => commands.app_bootstrap({}), 'rejects without a runtime')) as IpcError;
    assertEqual(e.code, 'INTERNAL', 'INTERNAL');
    assertMatch(e.detail ?? '', /^ui-/, 'correlation id');
  });
  assertEqual(out.log[0]?.kind, 'error', 'logged as an error-kind rejection');
});

test('IpcError rejections pass through unchanged; near-misses are not trusted', async () => {
  const real: IpcError = { code: 'BUSY', key: 'error.busy', params: { retryAfterMs: 120 }, detail: null, retryable: true };
  mockIPC(() => {
    throw real;
  });
  try {
    const e = (await assertRejects(() => commands.pty_resize({} as never), 'rejects')) as IpcError;
    assertDeepEqual(e, real, 'the core error is kept as it is');
  } finally {
    clearMocks();
  }
  await capture(async () => {
    const missingDetail = toIpcError({ code: 'NOT_FOUND', key: 'error.notFound', params: {}, retryable: false }, 'x');
    assertEqual(missingDetail.code, 'NOT_FOUND', 'a missing detail is accepted');
    assertEqual(missingDetail.detail, null, 'and normalised to null');
    assertEqual(toIpcError({ code: 'NOPE', key: 'k', params: {}, detail: null, retryable: false }, 'x').code, 'INTERNAL', 'unknown code');
    assertEqual(toIpcError({ code: 'BUSY', key: '', params: {}, detail: null, retryable: true }, 'x').code, 'INTERNAL', 'empty key');
    assertEqual(toIpcError({ code: 'BUSY', key: 'k', params: { a: {} }, detail: null, retryable: true }, 'x').code, 'INTERNAL', 'non-primitive param');
    assertEqual(toIpcError({ code: 'BUSY', key: 'k', params: [], detail: null, retryable: true }, 'x').code, 'INTERNAL', 'array params');
    assertEqual(toIpcError({ code: 'BUSY', key: 'k', params: {}, detail: 3, retryable: true }, 'x').code, 'INTERNAL', 'numeric detail');
    assertEqual(toIpcError(null, 'x').code, 'INTERNAL', 'null');
  });
});

test('the invoke arguments are exactly { req } (plus channel for streaming commands)', async () => {
  const seen: [string, unknown][] = [];
  mockIPC((cmd, payload) => {
    seen.push([cmd, payload]);
    return {};
  });
  try {
    await commands.app_mark({ mark: 'firstPaint', perfMs: 5 });
    assertEqual(seen[0]?.[0], 'app_mark', 'the command name');
    assertDeepEqual(Object.keys(seen[0]?.[1] as object), ['req'], 'only req');
    assertDeepEqual((seen[0]?.[1] as { req: unknown }).req, { mark: 'firstPaint', perfMs: 5 }, 'the request');
  } finally {
    clearMocks();
  }
});

test('every manifest command has a wrapper with the generated function behind it', () => {
  for (const name of COMMAND_NAMES) {
    assertEqual(typeof commands[name], 'function', `${name} is wrapped`);
    assertEqual(typeof (gen as Record<string, unknown>)[name], 'function', `${name} is generated`);
  }
  assertEqual(Object.keys(commands).length, COMMAND_NAMES.length, 'no extra wrappers');
  assert(Object.isFrozen(commands), 'the wrapper table cannot be patched at runtime');
});

test('showcase mode: the installed backend answers; without one the call is UNSUPPORTED', async () => {
  const core = fakeCore({ link_open: () => ({ opened: true }) });
  try {
    const res = await commands.link_open({ url: 'https://example.com/' });
    assertEqual(res.opened, true, 'answered by the backend');
    assertEqual(core.calls('link_open').length, 1, 'one call recorded');
    const missing = (await assertRejects(() => commands.app_quit_cancel({}), 'no handler')) as IpcError;
    assertEqual(missing.code, 'UNSUPPORTED', 'a missing handler is a stub refusal');
    installShowcaseBackend(null);
    const none = (await assertRejects(() => commands.app_bootstrap({}), 'no backend')) as IpcError;
    assertEqual(none.code, 'UNSUPPORTED', 'no backend → UNSUPPORTED');
    assertEqual(none.params.cap, 'showcase', 'named as the showcase');
  } finally {
    resetFakeCore();
  }
});

test('showcase mode: a backend that throws a string still yields INTERNAL', async () => {
  setShowcaseModeForTests(true);
  installShowcaseBackend(async () => {
    throw 'raw text with secret-canary';
  });
  try {
    const out = await capture(async () => {
      const e = (await assertRejects(() => commands.app_bootstrap({}), 'rejects')) as IpcError;
      assertEqual(e.code, 'INTERNAL', 'INTERNAL');
    });
    assert(!out.console.includes('secret-canary'), 'not logged');
  } finally {
    resetFakeCore();
  }
});
