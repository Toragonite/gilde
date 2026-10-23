/**
 * The terminal pool — module-level, LRU, capped at eight (contract §6.2.6).
 *
 * A `Terminal` outlives the React component that shows it: switching sub-views
 * or moving a session into a tile detaches its DOM node and re-attaches it,
 * never rebuilding the terminal (that would lose the scrollback the user was
 * reading). Only eviction (more than eight held, the oldest off screen) or
 * the session's end disposes it.
 *
 * The pool also owns the byte accounting:
 *  - `PtyChunk.offset` is the ring offset of the chunk's first byte; the
 *    snapshot header's `nextSeq` is the total the PTY has produced.
 *  - "Drop what we already have" is `end <= nextSeq`; a chunk that straddles
 *    the boundary is written from `nextSeq` on rather than dropped whole. The
 *    same arithmetic makes a resync snapshot write only the missing tail.
 *  - Attach first, then snapshot (§6.2.6): chunks that arrive in between are
 *    buffered and de-duplicated by offset once the snapshot lands.
 *
 * Bytes never enter React state.
 */

import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { Terminal } from '@xterm/xterm';

import { b64ToBytes } from '../lib/codec';
import type { PtyId } from '../lib/gen/PtyId';
import { readTerminalTheme, terminalOptions, watchTerminalTheme } from './terminalTheme';

export const POOL_LIMIT = 8;

/** A data chunk as the pool consumes it. */
export interface DataChunk {
  offset: number;
  b64: string;
}

export interface PooledTerminal {
  ptyId: PtyId;
  term: Pick<Terminal, 'write' | 'dispose' | 'open' | 'focus' | 'loadAddon' | 'onData' | 'onBinary' | 'cols' | 'rows' | 'options'>;
  fit: Pick<FitAddon, 'fit'>;
  container: HTMLDivElement;
  webgl: WebglAddon | null;
  opened: boolean;
  mounted: boolean;
  /** A channel is attached in the core and feeding this terminal. */
  attached: boolean;
  /** Another view took the PTY (`Detached{otherView}`): the view offers "Show here". */
  detached: boolean;
  /** Total bytes applied, in PTY stream coordinates. */
  nextSeq: number;
  /** Chunks that arrived before the first snapshot landed. */
  pending: DataChunk[];
  buffering: boolean;
  lastSize: { cols: number; rows: number } | null;
  lastUsed: number;
  disposers: (() => void)[];
  /** Listeners for `detached` changes (the view re-renders). */
  listeners: Set<() => void>;
}

const pool = new Map<PtyId, PooledTerminal>();
let useCounter = 0;
let themeWatcher: (() => void) | null = null;

// ---------------------------------------------------------------------------
// Snapshot wire format (`GLDP` header + ring)
// ---------------------------------------------------------------------------

export const SNAPSHOT_HEADER_LEN = 16;
export const SNAPSHOT_TAG = [0x47, 0x4c, 0x44, 0x50] as const;
export const SNAPSHOT_VERSION = 1;

export interface SnapshotHeader {
  version: number;
  /** Total bytes the PTY has produced; the offset after the ring's last byte. */
  nextSeq: number;
}

/** Parse the 16-byte header. Throws on a foreign or truncated buffer. */
export function readSnapshotHeader(bytes: Uint8Array): SnapshotHeader {
  if (bytes.length < SNAPSHOT_HEADER_LEN) throw new Error('pty snapshot: short header');
  for (let i = 0; i < SNAPSHOT_TAG.length; i++) {
    if (bytes[i] !== SNAPSHOT_TAG[i]) throw new Error('pty snapshot: bad tag bytes');
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return { version: bytes[4] as number, nextSeq: Number(view.getBigUint64(8, true)) };
}

export interface WriteSlice {
  from: number;
  nextSeq: number;
  /** Bytes were missed between what we had and what arrived. */
  gap: boolean;
}

/** How much of an incoming run of bytes is new (`null` = all old). */
export function writeSlice(nextSeq: number, start: number, length: number): WriteSlice | null {
  const end = start + length;
  if (end <= nextSeq) return null;
  if (start >= nextSeq) return { from: 0, nextSeq: end, gap: start > nextSeq };
  return { from: nextSeq - start, nextSeq: end, gap: false };
}

/** Write a chunk, or drop it if the view already has it. */
export function applyChunk(entry: PooledTerminal, chunk: DataChunk): void {
  if (entry.buffering) {
    entry.pending.push(chunk);
    return;
  }
  const bytes = b64ToBytes(chunk.b64);
  const slice = writeSlice(entry.nextSeq, chunk.offset, bytes.length);
  if (!slice) return;
  entry.term.write(slice.from > 0 ? bytes.subarray(slice.from) : bytes);
  entry.nextSeq = slice.nextSeq;
}

function drain(entry: PooledTerminal): void {
  entry.buffering = false;
  const pending = entry.pending;
  entry.pending = [];
  for (const chunk of pending) applyChunk(entry, chunk);
}

/**
 * Apply a `pty_snapshot` buffer: only the part of the ring the view has not
 * seen, then whatever arrived while it was in flight. A buffer without the
 * header is not a snapshot: it is ignored and the live chunks carry on.
 */
export function applySnapshot(entry: PooledTerminal, buffer: ArrayBuffer): { written: number; valid: boolean } {
  const bytes = new Uint8Array(buffer);
  let header: SnapshotHeader;
  try {
    header = readSnapshotHeader(bytes);
  } catch {
    drain(entry);
    return { written: 0, valid: false };
  }
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
  drain(entry);
  return { written, valid: true };
}

/** Start buffering again: the next snapshot decides what is new. */
export function beginAttach(entry: PooledTerminal): void {
  entry.buffering = true;
  entry.pending = [];
}

/** The snapshot failed: stop buffering so the live chunks are not held forever. */
export function endAttachWithoutSnapshot(entry: PooledTerminal): void {
  drain(entry);
}

export function setDetached(entry: PooledTerminal, detached: boolean): void {
  if (entry.detached === detached) return;
  entry.detached = detached;
  if (detached) entry.attached = false;
  for (const fn of [...entry.listeners]) fn();
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

export interface TerminalParts {
  term: PooledTerminal['term'];
  fit: PooledTerminal['fit'];
  container: HTMLDivElement;
}

export type TerminalFactory = () => TerminalParts;

const defaultFactory: TerminalFactory = () => {
  const term = new Terminal(terminalOptions());
  const fit = new FitAddon();
  term.loadAddon(fit);
  const container = document.createElement('div');
  container.className = 'g-term-surface';
  return { term, fit, container };
};

let factory: TerminalFactory = defaultFactory;

/** Tests: build terminals without xterm's renderer (`null` restores xterm). */
export function setTerminalFactory(next: TerminalFactory | null): void {
  factory = next ?? defaultFactory;
}

function makeEntry(ptyId: PtyId): PooledTerminal {
  const { term, fit, container } = factory();
  return {
    ptyId,
    term,
    fit,
    container,
    webgl: null,
    opened: false,
    mounted: false,
    attached: false,
    detached: false,
    nextSeq: 0,
    pending: [],
    buffering: true,
    lastSize: null,
    lastUsed: ++useCounter,
    disposers: [],
    listeners: new Set(),
  };
}

/**
 * WebGL, guarded: WKWebView drops the GL context under memory pressure, so a
 * context loss disposes the addon and falls back to the DOM renderer.
 */
function enableWebgl(entry: PooledTerminal): void {
  if (entry.webgl || factory !== defaultFactory) return;
  try {
    const addon = new WebglAddon();
    const off = addon.onContextLoss(() => {
      addon.dispose();
      entry.webgl = null;
    });
    entry.term.loadAddon(addon);
    entry.webgl = addon;
    entry.disposers.push(() => off.dispose());
  } catch {
    entry.webgl = null;
  }
}

function evict(): void {
  while (pool.size > POOL_LIMIT) {
    let victim: PooledTerminal | null = null;
    for (const entry of pool.values()) {
      if (entry.mounted) continue;
      if (!victim || entry.lastUsed < victim.lastUsed) victim = entry;
    }
    if (!victim) return;
    disposeTerminal(victim.ptyId);
  }
}

export function acquireTerminal(ptyId: PtyId): PooledTerminal {
  let entry = pool.get(ptyId);
  if (!entry) {
    entry = makeEntry(ptyId);
    pool.set(ptyId, entry);
  }
  entry.lastUsed = ++useCounter;
  entry.mounted = true;
  if (!themeWatcher && factory === defaultFactory) themeWatcher = watchTerminalTheme(retheme);
  evict();
  return entry;
}

/** Mount into a host element; the terminal is opened once and moved afterwards. */
export function mountTerminal(entry: PooledTerminal, host: HTMLElement): void {
  host.appendChild(entry.container);
  if (!entry.opened) {
    entry.term.open(entry.container);
    entry.opened = true;
    enableWebgl(entry);
  }
}

/** Detach from the host but keep the terminal and its channel. */
export function releaseTerminal(ptyId: PtyId): void {
  const entry = pool.get(ptyId);
  if (!entry) return;
  entry.mounted = false;
  entry.lastUsed = ++useCounter;
  entry.container.remove();
  evict();
}

export function disposeTerminal(ptyId: PtyId): void {
  const entry = pool.get(ptyId);
  if (!entry) return;
  pool.delete(ptyId);
  for (const dispose of entry.disposers) {
    try {
      dispose();
    } catch {
      /* already disposed */
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

export function retheme(): void {
  const theme = readTerminalTheme();
  for (const entry of pool.values()) entry.term.options.theme = theme;
}

export function pooledTerminal(ptyId: PtyId): PooledTerminal | undefined {
  return pool.get(ptyId);
}

export function pooledPtyIds(): PtyId[] {
  return [...pool.values()].sort((a, b) => a.lastUsed - b.lastUsed).map((e) => e.ptyId);
}

/** Tests only. */
export function resetPoolForTests(): void {
  for (const id of [...pool.keys()]) disposeTerminal(id);
}
