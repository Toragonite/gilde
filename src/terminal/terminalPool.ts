/**
 * The terminal pool — module-level, LRU, capped at eight (DESIGN §6-7, §9-4).
 *
 * A `Terminal` outlives the React component that shows it. Switching tabs,
 * changing the split or moving a tab to another pane detaches a DOM node and
 * re-attaches it; it never rebuilds the terminal, because rebuilding means
 * losing the scrollback the user was reading. **Only `session_close` disposes**
 * (`disposeTerminal`).
 *
 * The pool also owns the wire accounting, and that is the point of this file:
 *
 *  - `PtyChunk.seq` is a **byte offset**, not a frame counter — it is where the
 *    chunk's first byte sits in the session's byte stream. `next_seq` in the
 *    snapshot header is the total number of bytes the session has produced.
 *  - So "drop what we already have" is `chunk.end <= nextSeq`, and a chunk that
 *    straddles the boundary is written from `nextSeq` on rather than dropped
 *    whole. The same arithmetic makes a resync snapshot write only the missing
 *    tail instead of repainting two megabytes.
 *
 * Bytes never enter React state: a chunk is decoded once and handed straight to
 * `term.write(Uint8Array)` (DESIGN §9-3 rule 1).
 */

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { WebglAddon } from '@xterm/addon-webgl';

import { b64ToBytes } from '../lib/codec';
import type { PtyChunk, SessionId, TerminalConfig } from '../lib/ipc';
import { readTerminalTheme, terminalOptions, watchTerminalTheme } from './terminalTheme';

/** DESIGN §6-7: eight live terminals is the cap. */
export const POOL_LIMIT = 8;

export interface PooledTerminal {
  sessionId: SessionId;
  term: Terminal;
  fit: FitAddon;
  /**
   * The terminal's own parent. It is moved between hosts, never re-opened —
   * `Terminal.open()` on a second parent is not a supported way to keep state.
   */
  container: HTMLDivElement;
  webgl: WebglAddon | null;
  opened: boolean;
  mounted: boolean;
  /** A channel is attached in Rust and feeding this terminal. */
  attached: boolean;
  /** The session ended: never re-attach, and dispose as soon as it is hidden. */
  closed: boolean;
  /** Total bytes applied to this terminal, in session stream coordinates. */
  nextSeq: number;
  /** Chunks that arrived before the first snapshot landed. */
  pending: PtyChunk[];
  /** True until the first snapshot has been applied after an attach. */
  buffering: boolean;
  /** The `(cols, rows)` last sent to `session_resize`. */
  lastSize: { cols: number; rows: number } | null;
  lastUsed: number;
  disposers: (() => void)[];
}

const pool = new Map<SessionId, PooledTerminal>();
let clock = 0;
let themeWatcher: (() => void) | null = null;

// ---------------------------------------------------------------------------
// Snapshot wire format (DESIGN §6-3)
// ---------------------------------------------------------------------------

export const SNAPSHOT_HEADER_LEN = 16;
/** The four tag bytes `GLDP` that open every snapshot. */
export const SNAPSHOT_TAG = [0x47, 0x4c, 0x44, 0x50] as const;
export const SNAPSHOT_VERSION = 1;

export interface SnapshotHeader {
  version: number;
  /** Total bytes the session has produced; the byte after the ring's last. */
  nextSeq: number;
}

/** Parse the 16-byte header. Throws on a foreign or truncated buffer. */
export function readSnapshotHeader(bytes: Uint8Array): SnapshotHeader {
  if (bytes.length < SNAPSHOT_HEADER_LEN) throw new Error('pty snapshot: short header');
  for (let i = 0; i < SNAPSHOT_TAG.length; i++) {
    if (bytes[i] !== SNAPSHOT_TAG[i]) throw new Error('pty snapshot: bad tag bytes');
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    version: bytes[4],
    // Little-endian u64 at offset 8. A session would have to emit 9 PB before
    // this loses precision as a JS number.
    nextSeq: Number(view.getBigUint64(8, true)),
  };
}

export interface WriteSlice {
  /** Index into the incoming bytes to start writing at. */
  from: number;
  /** The `nextSeq` to record once the write happened. */
  nextSeq: number;
  /** Bytes were missed between what we had and what arrived. */
  gap: boolean;
}

/**
 * How much of an incoming run of bytes is new.
 *
 * `start` is the stream offset of the run's first byte. Returns `null` when the
 * run is entirely old — the "`seq < nextSeq` ⇒ drop" rule of DESIGN §6-3,
 * refined so a chunk that begins before `nextSeq` and ends after it keeps its
 * tail instead of being dropped whole.
 */
export function writeSlice(nextSeq: number, start: number, length: number): WriteSlice | null {
  const end = start + length;
  if (end <= nextSeq) return null;
  if (start >= nextSeq) return { from: 0, nextSeq: end, gap: start > nextSeq };
  return { from: nextSeq - start, nextSeq: end, gap: false };
}

// ---------------------------------------------------------------------------
// Applying bytes
// ---------------------------------------------------------------------------

/** Write a chunk into the terminal, or drop it if the view already has it. */
export function applyChunk(entry: PooledTerminal, chunk: PtyChunk): void {
  if (entry.buffering) {
    entry.pending.push(chunk);
    return;
  }
  const bytes = b64ToBytes(chunk.b64);
  const slice = writeSlice(entry.nextSeq, chunk.seq, bytes.length);
  if (!slice) return;
  entry.term.write(slice.from > 0 ? bytes.subarray(slice.from) : bytes);
  entry.nextSeq = slice.nextSeq;
}

export interface SnapshotResult {
  /** Bytes actually written to the terminal. */
  written: number;
  /** The session's total byte count as of the snapshot. */
  nextSeq: number;
  /** The ring was empty although the session has produced bytes. */
  scrollbackLost: boolean;
}

/**
 * Apply a `session_snapshot` buffer: header, then only the part of the ring the
 * view has not already seen. A first mount (`nextSeq === 0`) writes the whole
 * ring; a resync writes just the tail after the hole.
 */
export function applySnapshot(entry: PooledTerminal, buffer: ArrayBuffer): SnapshotResult {
  const bytes = new Uint8Array(buffer);
  const header = readSnapshotHeader(bytes);
  const body = bytes.subarray(SNAPSHOT_HEADER_LEN);
  const start = header.nextSeq - body.length;

  let written = 0;
  const slice = writeSlice(entry.nextSeq, start, body.length);
  if (slice) {
    const out = slice.from > 0 ? body.subarray(slice.from) : body;
    entry.term.write(out);
    written = out.length;
    entry.nextSeq = slice.nextSeq;
  } else if (header.nextSeq > entry.nextSeq) {
    entry.nextSeq = header.nextSeq;
  }

  // Drain whatever arrived while the snapshot was in flight; the byte
  // arithmetic above and in `applyChunk` is what makes this free of duplicates.
  entry.buffering = false;
  const pending = entry.pending;
  entry.pending = [];
  for (const chunk of pending) applyChunk(entry, chunk);

  return {
    written,
    nextSeq: header.nextSeq,
    scrollbackLost: body.length === 0 && header.nextSeq > 0,
  };
}

/** Start buffering again: the next snapshot decides what is new. */
export function beginAttach(entry: PooledTerminal): void {
  entry.buffering = true;
  entry.pending = [];
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

/**
 * How a terminal is built. Swapped by the unit test to exercise the LRU without
 * a DOM (the same seam `graphSlice` uses for `graph_snapshot()`); production
 * never calls the setter.
 */
export interface TerminalParts {
  term: Terminal;
  fit: FitAddon;
  container: HTMLDivElement;
}

export type TerminalFactory = (config: TerminalConfig) => TerminalParts;

const defaultFactory: TerminalFactory = (config) => {
  const term = new Terminal(terminalOptions(config));
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new WebLinksAddon());

  const container = document.createElement('div');
  container.className = 'term-surface';
  return { term, fit, container };
};

let factory: TerminalFactory = defaultFactory;

export function setTerminalFactory(next: TerminalFactory): void {
  factory = next;
}

export function resetTerminalFactory(): void {
  factory = defaultFactory;
}

function makeEntry(sessionId: SessionId, config: TerminalConfig): PooledTerminal {
  const { term, fit, container } = factory(config);

  return {
    sessionId,
    term,
    fit,
    container,
    webgl: null,
    opened: false,
    mounted: false,
    attached: false,
    closed: false,
    nextSeq: 0,
    pending: [],
    buffering: true,
    lastSize: null,
    lastUsed: ++clock,
    disposers: [],
  };
}

/**
 * WebGL, guarded (DESIGN §9-4). WKWebView drops the GL context under memory
 * pressure and an unguarded WebGL renderer paints an empty terminal at the
 * worst possible moment, so a context loss disposes the addon and falls back to
 * the DOM renderer.
 */
function enableWebgl(entry: PooledTerminal): void {
  if (entry.webgl) return;
  try {
    const addon = new WebglAddon();
    const off = addon.onContextLoss(() => {
      addon.dispose();
      entry.webgl = null;
      console.warn('[gilde] webgl context lost; falling back to the dom renderer');
    });
    entry.term.loadAddon(addon);
    entry.webgl = addon;
    entry.disposers.push(() => off.dispose());
  } catch (e) {
    // No WebGL in this webview: the DOM renderer is the fallback, not an error.
    entry.webgl = null;
    console.warn('[gilde] webgl renderer unavailable', e);
  }
}

function evict(): void {
  while (pool.size > POOL_LIMIT) {
    let victim: PooledTerminal | null = null;
    for (const entry of pool.values()) {
      if (entry.mounted) continue;
      if (!victim || entry.lastUsed < victim.lastUsed) victim = entry;
    }
    // Every survivor is on screen — the cap yields rather than blanking a pane.
    if (!victim) return;
    disposeTerminal(victim.sessionId);
  }
}

/** The pooled terminal for a session, created on first use. */
export function acquireTerminal(sessionId: SessionId, config: TerminalConfig): PooledTerminal {
  let entry = pool.get(sessionId);
  if (!entry) {
    entry = makeEntry(sessionId, config);
    pool.set(sessionId, entry);
  }
  entry.lastUsed = ++clock;
  entry.mounted = true;

  if (!themeWatcher) themeWatcher = watchTerminalTheme(retheme);
  evict();
  return entry;
}

/**
 * Mount the pooled terminal into a host element. Safe to call on every remount:
 * the terminal is opened exactly once, in its own container, and afterwards the
 * container is simply moved.
 */
export function mountTerminal(entry: PooledTerminal, host: HTMLElement, webgl: boolean): void {
  host.appendChild(entry.container);
  if (!entry.opened) {
    entry.term.open(entry.container);
    entry.opened = true;
    if (webgl) enableWebgl(entry);
  }
}

/** Detach from the host but keep the terminal, its scrollback and its channel. */
export function releaseTerminal(sessionId: SessionId): void {
  const entry = pool.get(sessionId);
  if (!entry) return;
  entry.mounted = false;
  entry.lastUsed = ++clock;
  entry.container.remove();
  if (entry.closed) {
    disposeTerminal(sessionId);
    return;
  }
  evict();
}

/**
 * The session is over (`session_close`). The terminal is disposed as soon as
 * nobody is looking at it — while a pane still shows it, the last screen the
 * agent painted stays readable, which is the whole reason a user stops a
 * session and then reads what it said.
 */
export function closeTerminal(sessionId: SessionId): void {
  const entry = pool.get(sessionId);
  if (!entry) return;
  entry.closed = true;
  if (!entry.mounted) disposeTerminal(sessionId);
}

/** The one thing that ends a terminal: `session_close` (DESIGN §6-7). */
export function disposeTerminal(sessionId: SessionId): void {
  const entry = pool.get(sessionId);
  if (!entry) return;
  pool.delete(sessionId);
  for (const dispose of entry.disposers) {
    try {
      dispose();
    } catch {
      /* a disposed addon is not an error */
    }
  }
  entry.webgl?.dispose();
  entry.term.dispose();
  entry.container.remove();
  if (pool.size === 0 && themeWatcher) {
    themeWatcher();
    themeWatcher = null;
  }
}

/** Repaint every live terminal after a theme change. */
export function retheme(): void {
  const theme = readTerminalTheme();
  for (const entry of pool.values()) entry.term.options.theme = theme;
}

/** Apply changed terminal settings to every live terminal. */
export function applyTerminalConfig(config: TerminalConfig): void {
  const options = terminalOptions(config);
  for (const entry of pool.values()) {
    entry.term.options.fontFamily = options.fontFamily;
    entry.term.options.fontSize = options.fontSize;
    entry.term.options.theme = options.theme;
    if (!config.webgl && entry.webgl) {
      entry.webgl.dispose();
      entry.webgl = null;
    }
  }
}

/** Sessions currently held, most recently used last. Diagnostics and tests. */
export function pooledSessionIds(): SessionId[] {
  return [...pool.values()].sort((a, b) => a.lastUsed - b.lastUsed).map((e) => e.sessionId);
}

export function pooledTerminal(sessionId: SessionId): PooledTerminal | undefined {
  return pool.get(sessionId);
}

export function terminalPoolSize(): number {
  return pool.size;
}
