/**
 * Terminal and Log sub-view tests (contract §4.2.4, §6.2.6): attach before
 * snapshot, offset de-duplication, resync, one attached view ("Show here"),
 * a headerless buffer never blocks live bytes, keys go to the PTY; the Log
 * shows facts per event and never the conversation's text.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { ChannelLike } from '../lib/channel';
import { textToB64 } from '../lib/codec';
import type { PtyChunk } from '../lib/gen/PtyChunk';
import type { PtyId } from '../lib/gen/PtyId';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { flush, withFakeCore } from '../test/backend';
import { block, text } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { describeEvent } from './LogView';
import {
  SNAPSHOT_HEADER_LEN,
  SNAPSHOT_TAG,
  applyChunk,
  applySnapshot,
  beginAttach,
  readSnapshotHeader,
  resetPoolForTests,
  setTerminalFactory,
  writeSlice,
} from './terminalPool';
import type { PooledTerminal } from './terminalPool';
import { TerminalView } from './TerminalView';

const PTY = 'p_1' as PtyId;
const decoder = new TextDecoder();

interface FakeTerm {
  written: string[];
  onData: ((d: string) => void) | null;
}

function fakeFactory(sink: FakeTerm) {
  return () => ({
    term: {
      write: (data: string | Uint8Array) => {
        sink.written.push(typeof data === 'string' ? data : decoder.decode(data));
      },
      dispose: () => {},
      open: () => {},
      focus: () => {},
      loadAddon: () => {},
      onData: (fn: (d: string) => void) => {
        sink.onData = fn;
        return { dispose: () => (sink.onData = null) };
      },
      onBinary: () => ({ dispose: () => {} }),
      cols: 80,
      rows: 24,
      options: {},
    } as unknown as PooledTerminal['term'],
    fit: { fit: () => {} },
    container: document.createElement('div'),
  });
}

function snapshotBuffer(body: string, nextSeq: number): ArrayBuffer {
  const bytes = new TextEncoder().encode(body);
  const out = new Uint8Array(SNAPSHOT_HEADER_LEN + bytes.length);
  out.set(SNAPSHOT_TAG, 0);
  out[4] = 1;
  new DataView(out.buffer).setBigUint64(8, BigInt(nextSeq), true);
  out.set(bytes, SNAPSHOT_HEADER_LEN);
  return out.buffer;
}

function entryFor(sink: FakeTerm): PooledTerminal {
  const parts = fakeFactory(sink)();
  return {
    ptyId: PTY,
    ...parts,
    webgl: null,
    opened: true,
    mounted: true,
    attached: false,
    detached: false,
    nextSeq: 0,
    pending: [],
    buffering: true,
    lastSize: null,
    lastUsed: 0,
    disposers: [],
    listeners: new Set(),
  };
}

test('writeSlice: old bytes dropped, straddling chunks trimmed, gaps reported', () => {
  assertEqual(writeSlice(10, 0, 10), null, 'all old');
  assertDeepEqual(writeSlice(10, 5, 10), { from: 5, nextSeq: 15, gap: false }, 'straddles');
  assertDeepEqual(writeSlice(10, 12, 3), { from: 0, nextSeq: 15, gap: true }, 'gap');
});

test('readSnapshotHeader rejects foreign buffers', () => {
  let threw = false;
  try {
    readSnapshotHeader(new TextEncoder().encode('hello world, not a snapshot'));
  } catch {
    threw = true;
  }
  assert(threw, 'bad tag');
  assertEqual(readSnapshotHeader(new Uint8Array(snapshotBuffer('abc', 3))).nextSeq, 3, 'nextSeq');
});

test('Attach then snapshot: chunks that arrive in between are de-duplicated by offset', () => {
  const sink: FakeTerm = { written: [], onData: null };
  const entry = entryFor(sink);
  beginAttach(entry);
  applyChunk(entry, { offset: 3, b64: textToB64('def') }); // arrived before the snapshot
  applySnapshot(entry, snapshotBuffer('abcde', 5)); // the ring already has "de"
  assertEqual(sink.written.join(''), 'abcdef', 'every byte once');
  applyChunk(entry, { offset: 4, b64: textToB64('efg') });
  assertEqual(sink.written.join(''), 'abcdefg', 'a straddling chunk writes only its tail');
});

test('A buffer without the snapshot header never blocks the live bytes', () => {
  const sink: FakeTerm = { written: [], onData: null };
  const entry = entryFor(sink);
  beginAttach(entry);
  applyChunk(entry, { offset: 0, b64: textToB64('$ claude') });
  const res = applySnapshot(entry, new TextEncoder().encode('$ claude').buffer as ArrayBuffer);
  assertEqual(res.valid, false, 'not a snapshot');
  assertEqual(sink.written.join(''), '$ claude', 'the buffered chunk is written');
});

test('Log lines are facts only: never the text of a block', () => {
  const secret = 'the user typed a secret plan';
  const lines = [
    describeEvent({ kind: 'appended', block: text('b1', 1, secret) }),
    describeEvent({ kind: 'appended', block: block('u1', 2, { kind: 'userText', text: secret, origin: 'human', delivery: 'queued', clientMsgId: null, images: [] }) }),
    describeEvent({ kind: 'updated', blockId: text('b1', 1, '').id, patch: { kind: 'textDelta', append: secret } }),
    describeEvent({ kind: 'reset', blocks: [text('b2', 3, secret)], hasOlder: false, reason: 'initial' }),
  ];
  for (const l of lines) assert(!l.includes('secret'), `no content: ${l}`);
  assertEqual(lines[2], `updated b1 textDelta +${secret.length}`, 'a delta is its length');
});

domTest('TerminalView: pty_attach before pty_snapshot; keys go to pty_write; Detached shows "Show here"', async () => {
  const sink: FakeTerm = { written: [], onData: null };
  setTerminalFactory(fakeFactory(sink));
  resetHotkeysForTests();
  appStore.setState({ ui: { ...initialUiState(), locale: 'en' } });
  let channel: ChannelLike<PtyChunk> | undefined;
  try {
    await withFakeCore(
      {
        pty_attach: (_req, ch) => {
          channel = ch as ChannelLike<PtyChunk>;
          return { offset: 0, cols: 80, rows: 24, title: null };
        },
        pty_snapshot: () => snapshotBuffer('hello', 5),
        pty_write: () => ({}),
        pty_resize: () => ({}),
        pty_detach: () => ({}),
      },
      async (core) => {
        const view = render(<TerminalView ptyId={PTY} label="claude terminal" active />);
        await act(async () => flush(10));
        assertDeepEqual(core.log.map((c) => c.name).filter((n) => n === 'pty_attach' || n === 'pty_snapshot'), ['pty_attach', 'pty_snapshot'], 'attach first');
        assertEqual(sink.written.join(''), 'hello', 'snapshot written');
        act(() => channel?.onmessage({ kind: 'data', offset: 5, b64: textToB64(' world') }));
        assertEqual(sink.written.join(''), 'hello world', 'live bytes');
        act(() => sink.onData?.('y'));
        await act(async () => flush());
        assertEqual(core.calls('pty_write').length, 1, 'typing goes to the PTY');
        act(() => channel?.onmessage({ kind: 'detached', reason: 'otherView' }));
        assert(view.getByText(translate('en', 'terminal.detached')) !== null, 'another view has it');
        fireEvent.click(view.getByText(translate('en', 'terminal.showHere')));
        await act(async () => flush(10));
        assertEqual(core.calls('pty_attach').length, 2, 'Show here attaches again');
      },
    );
  } finally {
    cleanup();
    resetPoolForTests();
    setTerminalFactory(null);
  }
});
