/**
 * The Terminal sub-view: the live PTY with its scrollback (ux-ia §2.2,
 * contract §4.2.4, §6.2.6).
 *
 * The attach order is not negotiable: `pty_attach` (the channel goes on
 * first), then `pty_snapshot`, then write only what the view has not seen —
 * chunks that arrived in between are de-duplicated by offset (the pool's
 * arithmetic). A `resync` chunk asks for a snapshot again. A PTY has one
 * attached view: when another view takes it, this one shows "Show here".
 *
 * React never sees a byte. Every key goes to the PTY except ⌘ chords, which
 * the hotkey layer still sees (`data-gilde-hotkeys="allow"`).
 */

import { useEffect, useReducer, useRef, useState } from 'react';

import '@xterm/xterm/css/xterm.css';

import { useResizeObserver } from '../hooks/useResizeObserver';
import { useI18n } from '../i18n/useI18n';
import { createChannel, disposeChannel } from '../lib/channel';
import { bytesToB64, textToB64 } from '../lib/codec';
import { commands } from '../lib/commands';
import type { PtyChunk } from '../lib/gen/PtyChunk';
import type { PtyId } from '../lib/gen/PtyId';
import type { ViewId } from '../lib/gen/ViewId';
import { newViewId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import {
  acquireTerminal,
  applyChunk,
  applySnapshot,
  beginAttach,
  endAttachWithoutSnapshot,
  mountTerminal,
  pooledTerminal,
  releaseTerminal,
  setDetached,
} from './terminalPool';
import type { PooledTerminal } from './terminalPool';
import './terminal.css';

const RESIZE_DEBOUNCE_MS = 60;

export interface TerminalViewProps {
  ptyId: PtyId;
  /** The accessible name ("claude terminal"). */
  label: string;
  /** Focus the terminal when it becomes the active view. */
  active?: boolean;
}

/** `onBinary` hands over a latin-1 string; it reaches the PTY as bytes. */
function latin1ToB64(data: string): string {
  const bytes = new Uint8Array(data.length);
  for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
  return bytesToB64(bytes);
}

/** Attach + snapshot for one pooled terminal (exported for tests). */
export async function attachTerminal(
  entry: PooledTerminal,
  viewId: ViewId,
  isLive: () => boolean,
): Promise<{ detach: () => void }> {
  let snapshotting = false;
  let again = false;
  const snapshot = async () => {
    if (snapshotting) {
      again = true;
      return;
    }
    snapshotting = true;
    try {
      do {
        again = false;
        try {
          const buffer = await commands.pty_snapshot({ ptyId: entry.ptyId });
          if (!isLive()) return;
          applySnapshot(entry, buffer);
        } catch {
          endAttachWithoutSnapshot(entry);
        }
      } while (again);
    } finally {
      snapshotting = false;
    }
  };
  // 1. the channel, before anything is read
  const channel = createChannel<PtyChunk>((chunk) => {
    const live = pooledTerminal(entry.ptyId);
    if (!live) return;
    switch (chunk.kind) {
      case 'data':
        applyChunk(live, chunk);
        return;
      case 'resync':
        beginAttach(live);
        void snapshot();
        return;
      case 'detached':
        setDetached(live, true);
        return;
    }
  });
  beginAttach(entry);
  const detach = () => {
    disposeChannel(channel);
    void commands.pty_detach({ ptyId: entry.ptyId, viewId }).catch(() => undefined);
  };
  try {
    await commands.pty_attach({ ptyId: entry.ptyId, viewId }, channel);
  } catch (e) {
    disposeChannel(channel);
    endAttachWithoutSnapshot(entry);
    throw e;
  }
  entry.attached = true;
  // The channel lives as long as the pooled terminal: disposing it detaches.
  entry.disposers.push(detach);
  setDetached(entry, false);
  // 2. and only then the ring
  await snapshot();
  return { detach };
}

export function TerminalView({ ptyId, label, active = false }: TerminalViewProps) {
  const { t, te } = useI18n();
  const hostRef = useRef<HTMLDivElement>(null);
  const entryRef = useRef<PooledTerminal | null>(null);
  const [failure, setFailure] = useState<IpcError | null>(null);
  const [epoch, reattach] = useReducer((n: number) => n + 1, 0);
  const [, rerender] = useReducer((n: number) => n + 1, 0);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const entry = acquireTerminal(ptyId);
    entryRef.current = entry;
    mountTerminal(entry, host);
    entry.listeners.add(rerender);
    let live = true;
    const viewId = newViewId('term');

    if (!entry.attached) {
      setFailure(null);
      attachTerminal(entry, viewId, () => live).then(undefined, (e: IpcError) => {
        if (live) setFailure(e);
      });
    }

    const offData = entry.term.onData((data) => {
      void commands.pty_write({ ptyId, b64: textToB64(data) }).catch(() => undefined);
    });
    const offBinary = entry.term.onBinary((data) => {
      void commands.pty_write({ ptyId, b64: latin1ToB64(data) }).catch(() => undefined);
    });
    const fitNow = () => fitAndResize(entry, ptyId, true);
    const frame = requestAnimationFrame(fitNow);

    return () => {
      live = false;
      cancelAnimationFrame(frame);
      offData.dispose();
      offBinary.dispose();
      entry.listeners.delete(rerender);
      // The pooled terminal keeps its channel while it is held, so returning
      // to this view is a DOM move and the scrollback stays where it was.
      releaseTerminal(ptyId);
      entryRef.current = null;
    };
  }, [ptyId, epoch]);

  useResizeObserver(
    hostRef,
    () => {
      const entry = entryRef.current;
      if (entry) fitAndResize(entry, ptyId, false);
    },
    RESIZE_DEBOUNCE_MS,
  );

  useEffect(() => {
    if (active) entryRef.current?.term.focus();
  }, [active]);

  const detached = entryRef.current?.detached ?? false;
  return (
    <div className="g-term" role="group" aria-label={label}>
      <div className="g-term-host" ref={hostRef} data-gilde-hotkeys="allow" />
      {detached && (
        <div className="g-term-over">
          <p>{t('terminal.detached')}</p>
          <button type="button" className="g-cbtn" onClick={reattach}>
            {t('terminal.showHere')}
          </button>
        </div>
      )}
      {failure && (
        <p className="g-term-failure" role="status">
          {te(failure)}
          {failure.detail ? <span className="mono"> {failure.detail}</span> : null}
        </p>
      )}
    </div>
  );
}

function fitAndResize(entry: PooledTerminal, ptyId: PtyId, force: boolean): void {
  if (!entry.container.isConnected) return;
  try {
    entry.fit.fit();
  } catch {
    return;
  }
  const cols = entry.term.cols;
  const rows = entry.term.rows;
  if (cols <= 0 || rows <= 0) return;
  if (!force && entry.lastSize && entry.lastSize.cols === cols && entry.lastSize.rows === rows) return;
  entry.lastSize = { cols, rows };
  void commands.pty_resize({ ptyId, cols, rows }).catch(() => undefined);
}
