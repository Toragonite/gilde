/**
 * Terminal lane tests (T20).
 *
 * The base64 round trip is acceptance 12 and is the load-bearing one: every
 * byte an agent prints crosses the IPC boundary inside a base64 envelope, and a
 * single mangled byte is a corrupted escape sequence, a broken CJK glyph or a
 * silently truncated line. It is exercised over 10,000 random buffers,
 * deliberately including invalid UTF-8.
 *
 * The rest pins the wire accounting that the DESIGN §6-3 remount sequence
 * depends on: `PtyChunk.seq` is a byte offset, so "already seen" is an interval
 * comparison, not `seq < nextSeq` on a frame counter.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertDeepEqual, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState } from '../test/lanes';
import { b64ToBytes, bytesToB64 } from '../lib/codec';
import {
  POOL_LIMIT,
  SNAPSHOT_HEADER_LEN,
  SNAPSHOT_VERSION,
  acquireTerminal,
  applyChunk,
  applySnapshot,
  beginAttach,
  closeTerminal,
  disposeTerminal,
  pooledSessionIds,
  readSnapshotHeader,
  releaseTerminal,
  resetTerminalFactory,
  setTerminalFactory,
  terminalPoolSize,
  writeSlice,
} from './terminalPool';
import type { TerminalParts } from './terminalPool';
import { ringDash, RING_CIRCUMFERENCE } from './SessionStatusPill';
import {
  confidenceBand,
  defaultTerminalConfig,
  isSessionOver,
  lastAgentFor,
  resolveActiveWorktree,
  viewOfInfo,
  withExit,
  withStatus,
} from '../store/sessionSlice';
import type { SessionView } from '../store/sessionSlice';
import type { AgentBinary, PtySessionInfo, SessionStatusEvent } from '../lib/ipc';
import { TerminalPane } from './TerminalPane';
// M1.5-D-sidebar-ui (BACKLOG 14, 17) and BACKLOG 15c (the focus target)
import { NewSessionActions, NewSessionForm, defaultFocusTarget } from './NewSessionDialog';
import type { NewSessionFocusTarget } from './NewSessionDialog';
// M1.5 integration (BACKLOG 14): the marker the dialog renders is now read by Sheet.
import { sheetFocusIndex } from '../components/Sheet';
// BACKLOG 15c: the focus path must hold no timer. The source is the only place
// that can be asserted about — a `setTimeout` that never fires leaves no trace
// in any rendered markup.
import newSessionDialogSource from './NewSessionDialog.tsx?raw';
import { terminalFontFamily, withCjkFallback } from './terminalTheme';
import { emptyGraph } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';

// ---------------------------------------------------------------------------
// A deterministic PRNG: a flake in a codec test is worse than no test.
// ---------------------------------------------------------------------------

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

test('base64 survives 10,000 random byte buffers, invalid UTF-8 included', () => {
  const random = mulberry32(0x5eed);
  let totalBytes = 0;
  for (let n = 0; n < 10_000; n++) {
    const length = Math.floor(random() * 512);
    const bytes = new Uint8Array(length);
    for (let i = 0; i < length; i++) bytes[i] = Math.floor(random() * 256);
    totalBytes += length;

    const back = b64ToBytes(bytesToB64(bytes));
    if (back.length !== bytes.length) {
      throw new Error(`buffer ${n}: length ${back.length} !== ${bytes.length}`);
    }
    for (let i = 0; i < length; i++) {
      if (back[i] !== bytes[i]) {
        throw new Error(`buffer ${n} byte ${i}: ${back[i]} !== ${bytes[i]}`);
      }
    }
  }
  assert(totalBytes > 1_000_000, 'the round trip covered a meaningful volume');
});

test('base64 survives a chunk larger than the encoder window', () => {
  // The encoder walks in 0x8000-byte slices; the boundary must not shift bytes.
  const bytes = new Uint8Array(0x8000 * 2 + 7);
  for (let i = 0; i < bytes.length; i++) bytes[i] = (i * 7 + (i >> 8)) & 0xff;
  const back = b64ToBytes(bytesToB64(bytes));
  assertEqual(back.length, bytes.length, 'length survives');
  for (let i = 0; i < bytes.length; i += 997) {
    assertEqual(back[i], bytes[i], `byte ${i} survives`);
  }
});

// ---------------------------------------------------------------------------
// Snapshot header (DESIGN §6-3)
// ---------------------------------------------------------------------------

function snapshotBuffer(nextSeq: number, body: Uint8Array): Uint8Array {
  const out = new Uint8Array(SNAPSHOT_HEADER_LEN + body.length);
  out.set([0x47, 0x4c, 0x44, 0x50], 0); // GLDP
  out[4] = SNAPSHOT_VERSION;
  new DataView(out.buffer).setBigUint64(8, BigInt(nextSeq), true);
  out.set(body, SNAPSHOT_HEADER_LEN);
  return out;
}

test('the snapshot header carries GLDP, a version and next_seq as u64 LE', () => {
  const header = readSnapshotHeader(snapshotBuffer(0x0102030405, new Uint8Array([1, 2, 3])));
  assertEqual(header.version, 1, 'version 1');
  assertEqual(header.nextSeq, 0x0102030405, 'next_seq round trips through the u64');
});

test('a foreign or truncated snapshot is rejected, not misread', () => {
  const bad = snapshotBuffer(10, new Uint8Array());
  bad[0] = 0x42;
  let threw = false;
  try {
    readSnapshotHeader(bad);
  } catch {
    threw = true;
  }
  assert(threw, 'a foreign tag throws');

  threw = false;
  try {
    readSnapshotHeader(new Uint8Array(8));
  } catch {
    threw = true;
  }
  assert(threw, 'a short buffer throws');
});

// ---------------------------------------------------------------------------
// Byte accounting (DESIGN §6-3 step 4)
// ---------------------------------------------------------------------------

test('a chunk the view already holds is dropped', () => {
  assertEqual(writeSlice(100, 0, 100), null, 'exactly what we have is dropped');
  assertEqual(writeSlice(100, 40, 20), null, 'a chunk strictly inside is dropped');
});

test('a contiguous chunk is written whole', () => {
  const slice = writeSlice(100, 100, 32);
  assert(slice !== null, 'not dropped');
  assertEqual(slice!.from, 0, 'written from its first byte');
  assertEqual(slice!.nextSeq, 132, 'next_seq advances by the length');
  assertEqual(slice!.gap, false, 'no gap');
});

test('a chunk straddling the snapshot boundary keeps its tail', () => {
  // The exact case the naive `seq < nextSeq` rule loses: a coalescer batch
  // flushed after the snapshot that began before it.
  const slice = writeSlice(100, 80, 40);
  assert(slice !== null, 'not dropped');
  assertEqual(slice!.from, 20, 'the first 20 bytes are already on screen');
  assertEqual(slice!.nextSeq, 120, 'next_seq is the chunk end');
});

test('a chunk past next_seq is written and reports the gap', () => {
  const slice = writeSlice(100, 140, 10);
  assert(slice !== null, 'not dropped');
  assertEqual(slice!.gap, true, 'the hole is visible to the caller');
  assertEqual(slice!.nextSeq, 150, 'accounting follows the stream, not the view');
});

test('a resync snapshot writes only the tail after the hole', () => {
  // ring holds [900, 1000), the view stopped at 950 → 50 bytes are new.
  const slice = writeSlice(950, 900, 100);
  assert(slice !== null, 'not dropped');
  assertEqual(slice!.from, 50, 'only the unseen tail is written');
  assertEqual(slice!.nextSeq, 1000, 'the view catches up to the ring');
});

test('a first mount writes the whole ring', () => {
  const slice = writeSlice(0, 4096, 2048);
  assert(slice !== null, 'not dropped');
  assertEqual(slice!.from, 0, 'everything retained is painted');
  assertEqual(slice!.nextSeq, 6144, 'and the view adopts the stream position');
});

// ---------------------------------------------------------------------------
// Confidence ring (DESIGN §6-4: below 0.5 the pill is hollow and dashed)
// ---------------------------------------------------------------------------

test('the confidence ring arc is proportional and hollow below 0.5', () => {
  const full = ringDash(1);
  assertEqual(full.hollow, false, 'certain is solid');
  assertEqual(Number(full.arc.split(' ')[0]), Number(RING_CIRCUMFERENCE.toFixed(2)), 'a full ring');

  const half = ringDash(0.5);
  assertEqual(half.hollow, false, '0.5 is the boundary and is not hollow');
  assert(Number(half.arc.split(' ')[0]) > 14 && Number(half.arc.split(' ')[0]) < 15, 'half arc');

  assertEqual(ringDash(0.49).hollow, true, 'below 0.5 the ring is hollow');
  assertEqual(ringDash(Number.NaN).hollow, true, 'a missing confidence is honest about it');
  assertEqual(ringDash(-1).arc.split(' ')[0], '0.00', 'clamped at zero');
});

test('confidence bands never claim false precision', () => {
  assertEqual(confidenceBand(0.95), 'high', 'high');
  assertEqual(confidenceBand(0.6), 'medium', 'medium');
  assertEqual(confidenceBand(0.4), 'low', 'low');
});

// ---------------------------------------------------------------------------
// Session view folding
// ---------------------------------------------------------------------------

function info(overrides: Partial<PtySessionInfo> = {}): PtySessionInfo {
  return {
    id: 'session-1',
    spawnId: 'spawn-1',
    projectId: 'project-1',
    worktreeId: 'worktree-1',
    agent: 'claude',
    accountId: null,
    title: 'claude · pty-manager',
    cwd: '/tmp/wt',
    cols: 80,
    rows: 24,
    pid: 41822,
    pgid: 41822,
    openedAt: 1000,
    detached: false,
    firstRetainedSeq: 0,
    nextSeq: 0,
    ...overrides,
  };
}

function statusEvent(overrides: Partial<SessionStatusEvent> = {}): SessionStatusEvent {
  return {
    sessionId: 'session-1',
    status: { kind: 'working', since: 2000 },
    confidence: 0.9,
    evidence: [{ kind: 'outputBurst', bytesPerS: 4096 }],
    since: 2000,
    at: 2001,
    ...overrides,
  };
}

test('a status event replaces status, confidence and evidence together', () => {
  const view = withStatus(viewOfInfo(info()), statusEvent());
  assertEqual(view.status.kind, 'working', 'status moved');
  assertEqual(view.confidence, 0.9, 'confidence moved with it');
  assertEqual(view.evidence.length, 1, 'evidence is never dropped on the floor');
  assertEqual(view.attention, false, 'no bell, no badge');
});

test('a bell raises the attention badge and it survives the next status', () => {
  let view = withStatus(viewOfInfo(info()), statusEvent({ evidence: [{ kind: 'bell' }] }));
  assertEqual(view.attention, true, 'BEL sets attention');
  view = withStatus(view, statusEvent({ evidence: [] }));
  assertEqual(view.attention, true, 'it stays until the tab is looked at');
});

test('an exit is terminal, fully confident and keeps the code', () => {
  const view = withExit(viewOfInfo(info()), {
    sessionId: 'session-1',
    pid: 41822,
    exitCode: 130,
    signal: null,
    at: 5000,
  });
  assertEqual(view.status.kind, 'exited', 'status is exited');
  assertEqual(view.confidence, 1, 'a process that exited is not a guess');
  assertEqual(view.exit?.code, 130, 'the code is kept for the inspector');
  assertEqual(isSessionOver(view.status), true, 'the composer disables itself');
});

test('a session_list row keeps the status a live session already had', () => {
  const first = withStatus(viewOfInfo(info()), statusEvent());
  const again = viewOfInfo(info({ rows: 40 }), first);
  assertEqual(again.status.kind, 'working', 'a refresh does not reset the status');
  assertEqual(again.info.rows, 40, 'but the facts are refreshed');
});

// ---------------------------------------------------------------------------
// ⌘T prefill (acceptance 10)
// ---------------------------------------------------------------------------

function sessionMap(views: SessionView[]): Map<string, SessionView> {
  return new Map(views.map((view) => [view.id, view]));
}

test('the new session dialog prefills the selected worktree', () => {
  const sessions = sessionMap([viewOfInfo(info())]);
  assertEqual(
    resolveActiveWorktree({
      selectedNodeId: 'worktree:wt-9',
      focusedSessionId: null,
      sessions,
      worktreeIds: ['wt-1'],
    }),
    'wt-9',
    'an explicit selection wins',
  );
});

test('it falls back to the focused session, then the newest, then the only one', () => {
  const sessions = sessionMap([
    viewOfInfo(info({ id: 'a', worktreeId: 'wt-a', openedAt: 10 })),
    viewOfInfo(info({ id: 'b', worktreeId: 'wt-b', openedAt: 20 })),
  ]);

  assertEqual(
    resolveActiveWorktree({
      selectedNodeId: null,
      focusedSessionId: 'a',
      sessions,
      worktreeIds: ['wt-z'],
    }),
    'wt-a',
    'the pane being looked at decides',
  );

  assertEqual(
    resolveActiveWorktree({
      selectedNodeId: null,
      focusedSessionId: null,
      sessions,
      worktreeIds: ['wt-z'],
    }),
    'wt-b',
    'otherwise the most recently opened session',
  );

  assertEqual(
    resolveActiveWorktree({
      selectedNodeId: null,
      focusedSessionId: null,
      sessions: new Map(),
      worktreeIds: ['wt-z'],
    }),
    'wt-z',
    'and finally the only worktree there is',
  );
});

test('the agent prefill is the one last used in that worktree', () => {
  const sessions = sessionMap([
    viewOfInfo(info({ id: 'a', worktreeId: 'wt-a', agent: 'codex', openedAt: 10 })),
    viewOfInfo(info({ id: 'b', worktreeId: 'wt-a', agent: 'claude', openedAt: 30 })),
    viewOfInfo(info({ id: 'c', worktreeId: 'wt-b', agent: 'agy', openedAt: 40 })),
  ]);
  assertEqual(lastAgentFor({}, sessions, 'wt-a'), 'claude', 'newest in this worktree');
  assertEqual(lastAgentFor({}, sessions, 'wt-b'), 'agy', 'and not the newest overall');
  assertEqual(lastAgentFor({ 'wt-b': 'codex' }, sessions, 'wt-b'), 'codex', 'the record wins');
  assertEqual(lastAgentFor({}, sessions, 'wt-new'), null, 'an untouched worktree has no default');
});

// ---------------------------------------------------------------------------
// The pane itself, rendered to static markup (no DOM, no Tauri host)
// ---------------------------------------------------------------------------

function seedSession(view: SessionView): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    sessions: new Map([[view.id, view]]),
  });
  // zustand v5 renders statically from the initial snapshot.
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

function renderPane(): string {
  return renderToStaticMarkup(
    <TerminalPane
      descriptor={{ view: 'terminal', sessionId: 'session-1' }}
      tab={{
        id: 'tab-1',
        title: 'claude',
        descriptor: { view: 'terminal', sessionId: 'session-1' },
        closable: true,
      }}
      paneId="pane-1"
      active
    />,
  );
}

test('the pane header renders the agent, the status pill and the composer', () => {
  seedSession(withStatus(viewOfInfo(info()), statusEvent()));
  const html = renderPane();

  assert(html.includes('class="hdr term-hdr"'), 'the 32px header rendered');
  assert(html.includes('>claude<'), 'the agent text badge, not a vendor logo');
  assert(html.includes('작업 중'), 'the status pill carries the Korean status text');
  assert(html.includes('class="ring"'), 'the confidence ring is drawn');
  assert(html.includes('var(--st-working)'), 'the ring colour is a token, never a literal');
  assert(html.includes('claude에게 보낼 메시지…'), 'the composer placeholder is Korean');
  assert(html.includes('⌘↵'), 'the send shortcut is shown');
  assert(html.includes('class="term-host"'), 'the terminal host rendered');
  assert(html.includes('data-gilde-hotkeys="allow"'), '⌘-hotkeys still work over xterm');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('a low confidence status draws the pill hollow', () => {
  seedSession(withStatus(viewOfInfo(info()), statusEvent({ confidence: 0.4 })));
  assert(renderPane().includes('data-hollow="true"'), 'the reader sees that this is a guess');
});

test('waiting for input replaces the paragraph with literal keys', () => {
  seedSession(
    withStatus(
      viewOfInfo(info()),
      statusEvent({
        status: { kind: 'waitingForInput', prompt: 'Do you trust the files in this folder? [y/N]' },
        confidence: 0.85,
      }),
    ),
  );
  const html = renderPane();
  assert(html.includes('class="term-keys"'), 'the literal key row rendered');
  assert(html.includes('Do you trust the files in this folder? [y/N]'), 'the matched line is shown');
  assert(html.includes('>y</span>') && html.includes('>n</span>'), 'y and n are one click away');
  assert(html.includes('Esc'), 'and so is Esc');
  assert(html.includes('입력 대기'), 'the pill agrees with the composer');
});

test('a session that is gone is stated, not faked', () => {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    sessions: new Map(),
  });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  const html = renderPane();
  assert(html.includes('이 세션은 더 이상 존재하지 않습니다.'), 'one honest sentence');
  assert(html.includes('새 세션'), 'and exactly one action');
});

// ---------------------------------------------------------------------------
// The pool: eight terminals, LRU, and only `session_close` disposes
// ---------------------------------------------------------------------------

interface FakeTerminal {
  disposed: boolean;
  written: number[][];
}

const fakes = new Map<string, FakeTerminal>();
let lastFake: FakeTerminal | null = null;

function installFakeTerminals(): void {
  fakes.clear();
  setTerminalFactory(() => {
    const fake: FakeTerminal = { disposed: false, written: [] };
    const term = {
      options: {},
      dispose: () => {
        fake.disposed = true;
      },
      write: (bytes: Uint8Array) => fake.written.push([...bytes]),
      loadAddon: () => {},
    };
    const container = { remove: () => {}, isConnected: false };
    // The pool only ever touches this much of each object.
    fakes.set(String(fakes.size), fake);
    lastFake = fake;
    return {
      term: term as unknown as TerminalParts['term'],
      fit: {} as TerminalParts['fit'],
      container: container as unknown as HTMLDivElement,
    };
  });
}

test('the pool keeps a terminal across mounts and only a close disposes it', () => {
  installFakeTerminals();
  try {
    const config = defaultTerminalConfig();
    const first = acquireTerminal('s1', config);
    first.nextSeq = 4096; // stands in for the scrollback the user is reading
    releaseTerminal('s1');
    assert(pooledSessionIds().includes('s1'), 'unmounting keeps the terminal alive');

    const again = acquireTerminal('s1', config);
    assert(again === first, 'the same terminal comes back');
    assertEqual(again.nextSeq, 4096, 'and it did not lose its place in the stream');

    // Closing while it is on screen leaves the final output readable…
    closeTerminal('s1');
    assert(pooledSessionIds().includes('s1'), 'still there while a pane shows it');
    // …and the terminal dies with the session as soon as the pane goes away.
    releaseTerminal('s1');
    assert(!pooledSessionIds().includes('s1'), 'session_close is what disposes');
  } finally {
    resetTerminalFactory();
    for (const id of pooledSessionIds()) disposeTerminal(id);
  }
});

test('the pool evicts the least recently used hidden terminal past eight', () => {
  installFakeTerminals();
  try {
    const config = defaultTerminalConfig();
    for (let i = 0; i < POOL_LIMIT; i++) {
      acquireTerminal(`s${i}`, config);
      releaseTerminal(`s${i}`);
    }
    assertEqual(terminalPoolSize(), POOL_LIMIT, 'the cap is eight');

    // Touch s0 so it is no longer the oldest.
    acquireTerminal('s0', config);
    releaseTerminal('s0');

    acquireTerminal('s8', config);
    assertEqual(terminalPoolSize(), POOL_LIMIT, 'still eight after the ninth');
    assert(!pooledSessionIds().includes('s1'), 'the least recently used one went');
    assert(pooledSessionIds().includes('s0'), 'the recently used one stayed');
    assert(pooledSessionIds().includes('s8'), 'and the new one is in');
  } finally {
    resetTerminalFactory();
    for (const id of pooledSessionIds()) disposeTerminal(id);
  }
});

test('a mounted terminal is never evicted, even over the cap', () => {
  installFakeTerminals();
  try {
    const config = defaultTerminalConfig();
    for (let i = 0; i < POOL_LIMIT + 2; i++) acquireTerminal(`m${i}`, config); // all mounted
    assertEqual(terminalPoolSize(), POOL_LIMIT + 2, 'the cap yields rather than blanking a pane');
  } finally {
    resetTerminalFactory();
    for (const id of pooledSessionIds()) disposeTerminal(id);
  }
});

test('attach → snapshot → write → drop is byte exact, in that order', () => {
  installFakeTerminals();
  try {
    const entry = acquireTerminal('s-remount', defaultTerminalConfig());
    const fake = lastFake!;

    // 1. the channel is attached first, so chunks start arriving immediately…
    beginAttach(entry);
    applyChunk(entry, { seq: 1000, b64: bytesToB64(new Uint8Array([0xf0, 0x9f])) });
    assertEqual(fake.written.length, 0, 'nothing is painted before the snapshot');

    // 2. …then the ring is read. It holds [960, 1000) and the chunk continues it.
    const ring = new Uint8Array(40);
    for (let i = 0; i < ring.length; i++) ring[i] = i;
    const buffer = snapshotBuffer(1000, ring);
    const result = applySnapshot(entry, buffer.buffer as ArrayBuffer);

    assertEqual(result.written, 40, 'the whole ring is painted on a first mount');
    assertEqual(fake.written.length, 2, 'ring first, then the buffered chunk');
    assertEqual(fake.written[0].length, 40, 'the ring went in whole');
    assertDeepEqual(fake.written[1], [0xf0, 0x9f], 'the split multi-byte pair is not duplicated');
    assertEqual(entry.nextSeq, 1002, 'the view is exactly where the stream is');

    // 3. a chunk the snapshot already contained is dropped
    applyChunk(entry, { seq: 980, b64: bytesToB64(new Uint8Array([1, 2, 3])) });
    assertEqual(fake.written.length, 2, 'no repaint of what is already on screen');

    // 4. a resync snapshot writes only the tail after the hole
    const later = new Uint8Array(30);
    later.fill(7);
    applySnapshot(entry, (snapshotBuffer(1012, later).buffer as ArrayBuffer));
    assertEqual(fake.written.length, 3, 'the resync painted once');
    assertEqual(fake.written[2].length, 10, 'and only the ten bytes that were missing');
    assertEqual(entry.nextSeq, 1012, 'the view caught up');
  } finally {
    resetTerminalFactory();
    for (const id of pooledSessionIds()) disposeTerminal(id);
  }
});

// ---------------------------------------------------------------------------
// M1.5 BACKLOG 11 — the composer has an accessible name
//
// The AX tree of the packaged app showed `AXTextArea [missing value]` for this
// field: a placeholder is not an accessible name, so VoiceOver announced
// nothing and scripts/gui-smoke had no name to address the composer by. The
// regression guard is two-part — this render test pins the attribute, and
// scripts/lint-ui.sh rule ui/a11y-input-label refuses any new unnamed control.
// ---------------------------------------------------------------------------

test('the composer textarea carries an accessible name, not just a placeholder', () => {
  seedSession(withStatus(viewOfInfo(info()), statusEvent()));
  const html = renderPane();

  assert(
    html.includes('aria-label="claude에게 보낼 메시지 입력란"'),
    'the composer textarea has an aria-label naming the agent it writes to',
  );
  const tag = html.slice(html.indexOf('<textarea'), html.indexOf('</textarea>'));
  assert(tag.includes('aria-label='), 'the name is on the textarea itself, not on a sibling');
  assert(tag.includes('placeholder='), 'and the placeholder is still there for a sighted user');
});

// ---------------------------------------------------------------------------
// M1.5 BACKLOG 14 / 15c — the new-session dialog opens on 세션 열기, not 취소,
// and not on a timer
//
// `Sheet` focuses its first action, which is Cancel by convention: right for a
// destructive sheet (UI-GUIDELINES §2-5), wrong for the dialog whose entire
// purpose is to open a session. The first fix marked the primary and left a
// `setTimeout(0)` to move the focus once the prefill had committed — and the
// 2026-09-09 hands-on session found the ring on the worktree select, because
// that timer was racing the agent roster and losing.
//
// So the decision is now made *during the render that Sheet focuses from*:
// `defaultFocusTarget` picks the control, the form marks exactly that one, and
// `sheetFocusIndex` picks it back out of the rendered markup. There is no DOM
// on Node, so the three are tested as they compose — which is also the order
// they run in.
// ---------------------------------------------------------------------------

test('the dialog focuses its primary action, and the first missing field when it cannot', () => {
  assertEqual(
    defaultFocusTarget({ worktreeSelected: true, agentReady: true }),
    'primary',
    'a prefilled form opens on 세션 열기, so Enter opens a session',
  );
  assertEqual(
    defaultFocusTarget({ worktreeSelected: false, agentReady: true }),
    'worktree',
    'with no worktree the focus goes to the field that is missing',
  );
  assertEqual(
    defaultFocusTarget({ worktreeSelected: true, agentReady: false }),
    'agent',
    'and with no installed agent chosen, to the agent row',
  );
  assertEqual(
    defaultFocusTarget({ worktreeSelected: false, agentReady: false }),
    'worktree',
    'the first missing field wins, in the order the form asks for them',
  );
});

/**
 * `Sheet`'s focus pass, run against real markup: the candidates it queries are
 * `[data-default-focus="true"]` anywhere in the dialog plus the action buttons,
 * in document order. This rebuilds that list from the rendered HTML of the body
 * and the actions, so the test exercises the same string the browser would.
 */
function focusedControl(bodyHtml: string, actionsHtml: string): string {
  const candidates: { marked: boolean; disabled: boolean; name: string }[] = [];
  const push = (html: string, onlyMarked: boolean) => {
    for (const tag of html.match(/<(?:button|select|textarea|input)[^>]*>/g) ?? []) {
      const marked = tag.includes('data-default-focus="true"');
      if (onlyMarked && !marked) continue;
      candidates.push({
        marked,
        disabled: /\sdisabled(?![-\w])/.test(tag),
        name: /class="([^"]*)"/.exec(tag)?.[1] ?? tag,
      });
    }
  };
  push(bodyHtml, true);
  push(actionsHtml, false);
  const index = sheetFocusIndex(candidates);
  return index < 0 ? 'nothing' : candidates[index].name;
}

/** An `agents_detect` row, shaped exactly like the one Rust answers with. */
function agentBinary(kind: AgentBinary['kind'], installed: boolean): AgentBinary {
  return {
    kind,
    absPath: installed ? `/usr/local/bin/${kind}` : null,
    version: installed ? '2.1.263' : null,
    detectedAt: 1,
    installed,
    ok: installed,
    verified: installed,
    verifiedRange: null,
    source: installed ? 'detected' : 'notFound',
  };
}

/** The launch form as the dialog renders it, for one form state. */
function formHtml(
  focusTarget: NewSessionFocusTarget,
  patch: Partial<Parameters<typeof NewSessionForm>[0]> = {},
): string {
  return renderToStaticMarkup(
    <NewSessionForm
      worktrees={[
        {
          id: 'worktree:w1',
          kind: 'worktree',
          parentId: 'project:p1',
          label: 'pty-manager',
          status: 'clean',
          badges: [],
          metrics: {},
          meta: {},
          updatedAt: 1,
        },
      ]}
      projects={new Map()}
      worktreeId="w1"
      agent="claude"
      prompt=""
      agentRows={[agentBinary('claude', true)]}
      focusTarget={focusTarget}
      onWorktreeChange={() => {}}
      onAgentChange={() => {}}
      onPromptChange={() => {}}
      onDetect={() => {}}
      onSubmit={() => {}}
      {...patch}
    />,
  );
}

test('a prefilled dialog hands the focus to 세션 열기 on the render Sheet reads', () => {
  // The ordinary case: worktree and agent prefilled, so the form is valid on
  // the very first commit and the primary is both enabled and marked.
  const body = formHtml('primary');
  const actions = renderToStaticMarkup(
    <NewSessionActions busy={false} canSubmit onCancel={() => {}} onSubmit={() => {}} />,
  );
  assert(
    !body.includes('data-default-focus'),
    'a valid form marks nothing in its body — the primary is the target',
  );
  assertEqual(focusedControl(body, actions), 'btn primary', 'and Sheet focuses 세션 열기');
});

test('an incomplete dialog hands the focus to the field it is incomplete because of', () => {
  // No worktree chosen: the primary cannot be focused (it is disabled), and
  // Cancel is not where the user has to go — the select is.
  const noWorktree = formHtml('worktree', { worktreeId: '' });
  const blocked = renderToStaticMarkup(
    <NewSessionActions busy={false} canSubmit={false} onCancel={() => {}} onSubmit={() => {}} />,
  );
  const select = /<select[^>]*>/.exec(noWorktree)?.[0] ?? '';
  assert(select.includes('data-default-focus="true"'), 'the worktree select is the target');
  assertEqual(focusedControl(noWorktree, blocked), 'input', 'and Sheet focuses it, not Cancel');

  // An agent that is not installed cannot launch anything, so the agent row is
  // the next thing to fix — and the marker lands on a chip that can take focus.
  const noAgent = formHtml('agent', {
    agent: 'codex',
    agentRows: [agentBinary('claude', true), agentBinary('codex', false)],
  });
  const chips: string[] = noAgent.match(/<button[^>]*class="chip"[^>]*>/g) ?? [];
  assertEqual(chips.length, 2, 'both agents are offered');
  assert(chips[0]?.includes('data-default-focus="true"'), 'the installed one is the target');
  assert(!chips[1]?.includes('data-default-focus'), 'and the one that cannot run is not');
  assertEqual(focusedControl(noAgent, blocked), 'chip', 'Sheet focuses the installed chip');
});

test('nothing in the focus path waits for a timer', () => {
  // The regression itself: the prefill used to land one commit after `Sheet`
  // had already chosen, and a `setTimeout(0)` was left to correct it. The
  // prefill is a render-phase update now, so the first DOM is the prefilled
  // one and there is no timer to lose a race.
  // Comment bodies are stripped first: the note above the prefill names the
  // timer it replaced, and a test that cannot tell prose from code would be
  // satisfied by deleting the explanation instead of the timer.
  const code = newSessionDialogSource
    .replace(/\/\*[\s\S]*?\*\//g, ' ')
    .replace(/^\s*\/\/.*$/gm, ' ');
  assert(
    !/setTimeout|requestAnimationFrame|queueMicrotask/.test(code),
    'the dialog schedules no deferred focus move',
  );
  assert(
    /if \(open !== prefilled\)/.test(newSessionDialogSource),
    'the prefill happens during render, before Sheet focuses anything',
  );
});

test('Sheet honours the default-focus marker, and falls back to Cancel', () => {
  // M1.5 integration: the marker used to be decorative — `Sheet` always focused
  // the first action, so only the dialog's own deferred effect moved focus, and
  // deleting that effect would have left every test green. `Sheet` now reads the
  // marker itself, and this is the rule it reads it by.
  const cancel = { marked: false, disabled: false };
  const primary = { marked: true, disabled: false };

  assertEqual(
    sheetFocusIndex([cancel, primary]),
    1,
    'a dialog that marked its primary opens on the primary, not on Cancel',
  );
  assertEqual(
    sheetFocusIndex([cancel, { marked: true, disabled: true }]),
    0,
    'a marked control that cannot be submitted yet does not take focus',
  );
  assertEqual(
    sheetFocusIndex([cancel, { marked: false, disabled: false }]),
    0,
    'an unmarked sheet keeps the destructive default: Cancel first (UI-GUIDELINES §2-5)',
  );
  assertEqual(sheetFocusIndex([]), -1, 'and a sheet with no actions focuses nothing');
});

test('the new session dialog marks the primary button as the default focus', () => {
  const html = renderToStaticMarkup(
    <NewSessionActions busy={false} canSubmit onCancel={() => {}} onSubmit={() => {}} />,
  );

  assert(html.indexOf('취소') < html.indexOf('세션 열기'), 'Cancel is still drawn first');
  const primary = html.slice(html.indexOf('class="btn primary"'));
  assert(primary.includes('data-default-focus="true"'), 'the primary action is the focus target');
  assert(
    !html.slice(0, html.indexOf('class="btn primary"')).includes('data-default-focus'),
    'and Cancel is not marked as one',
  );
  assert(!primary.includes('disabled'), 'a submittable form offers it enabled');

  const blocked = renderToStaticMarkup(
    <NewSessionActions busy={false} canSubmit={false} onCancel={() => {}} onSubmit={() => {}} />,
  );
  assert(
    blocked.slice(blocked.indexOf('class="btn primary"')).includes('disabled'),
    'and an incomplete form still refuses to submit',
  );
});

// ---------------------------------------------------------------------------
// M1.5 BACKLOG 17 — Korean in the terminal needs a CJK fixed-pitch face
// ---------------------------------------------------------------------------

test('the terminal font stack prefers CJK monospaces and never leads with a generic', () => {
  const stack = terminalFontFamily({
    fontSize: 12,
    fontFamily: '',
    scrollbackBytes: 1,
    webgl: false,
    shell: null,
  });
  const names = stack.split(',').map((s) => s.trim().replace(/'/g, ''));

  assertEqual(names[0], 'D2Coding', 'the recommended Korean coding font is tried first');
  assertDeepEqual(
    names.slice(0, 4),
    ['D2Coding', 'Sarasa Mono K', 'Sarasa Term K', 'Noto Sans Mono CJK KR'],
    'then the other CJK fixed-pitch families, in order',
  );
  assert(
    names.indexOf('SF Mono') > names.indexOf('Noto Sans Mono CJK KR'),
    'the Latin monospaces follow them',
  );
  assert(
    names.indexOf('Apple SD Gothic Neo') > names.indexOf('Menlo'),
    'the proportional Korean face is the last resort, behind Menlo — leading with it ' +
      'would hand ASCII to a proportional font on every machine with no CJK monospace',
  );
  assertEqual(names[names.length - 1], 'monospace', 'a generic family can only come last');
});

test('a configured font keeps ASCII and only gains the CJK fallbacks', () => {
  const names = withCjkFallback("'JetBrains Mono', monospace", false)
    .split(',')
    .map((s) => s.trim().replace(/'/g, ''));
  assertEqual(names[0], 'JetBrains Mono', "the user's choice still draws the grid");
  assertEqual(names[1], 'D2Coding', 'the CJK faces sit behind it, for the glyphs it lacks');
  assertEqual(names[names.length - 1], 'monospace', 'the generic is still last');

  const deduped = withCjkFallback("'D2Coding', Menlo", false).split(',').map((s) => s.trim());
  assertEqual(
    deduped.filter((name) => name.replace(/'/g, '') === 'D2Coding').length,
    1,
    'a family the user already named is not listed twice',
  );
});
