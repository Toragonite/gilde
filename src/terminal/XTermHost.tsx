/**
 * XTermHost — the imperative island (DESIGN §9-4).
 *
 * The remount sequence closes the race, and the order is not negotiable
 * (DESIGN §6-3):
 *
 *   1. `session_attach(id, channel)` — the channel goes on **first**
 *   2. `session_snapshot(id)`        — then the ring is read
 *   3. write the snapshot body
 *   4. chunks the view already holds are dropped by byte offset
 *
 * Taking the snapshot first would lose everything the agent printed in between.
 *
 * React never sees a byte: the channel callback decodes base64 and calls
 * `term.write(Uint8Array)` inside the pool. The only React state here is a
 * mount-level error string, so a flood at 49 MB/s produces exactly zero
 * re-renders (DESIGN §9-3 rule 1).
 */

import { Channel } from '@tauri-apps/api/core';
import { useEffect, useRef, useState } from 'react';

import '@xterm/xterm/css/xterm.css';

import { onGildeEvent } from '../hooks/useEventBridge';
import { useResizeObserver } from '../hooks/useResizeObserver';
import { useI18n } from '../i18n/useI18n';
import { bytesToB64, textToB64 } from '../lib/codec';
import { asGildeError, cmd, isUnavailable } from '../lib/commands';
import { PTY_RESYNC } from '../lib/ipc';
import type { PtyChunk, SessionId } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import {
  acquireTerminal,
  applySnapshot,
  applyChunk,
  applyTerminalConfig,
  beginAttach,
  mountTerminal,
  pooledTerminal,
  releaseTerminal,
} from './terminalPool';
import type { PooledTerminal } from './terminalPool';

/** DESIGN §9-4: fit is debounced, the pty is only told when the grid changed. */
const RESIZE_DEBOUNCE_MS = 60;

export interface XTermHostProps {
  sessionId: SessionId;
  /** Focus the terminal when its pane becomes the active one. */
  active?: boolean;
}

/** `onBinary` hands over a latin-1 string; it must reach the pty as bytes. */
function latin1ToB64(data: string): string {
  const bytes = new Uint8Array(data.length);
  for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
  return bytesToB64(bytes);
}

export function XTermHost({ sessionId, active }: XTermHostProps) {
  const { t } = useI18n();
  const hostRef = useRef<HTMLDivElement>(null);
  const entryRef = useRef<PooledTerminal | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  const terminalConfig = useAppStore((s) => s.terminalConfig);
  const ensureTerminalConfig = useAppStore((s) => s.ensureTerminalConfig);
  const markScrollbackLost = useAppStore((s) => s.markScrollbackLost);
  const setAttention = useAppStore((s) => s.setAttention);
  const pushErrorToast = useAppStore((s) => s.pushErrorToast);

  useEffect(() => {
    void ensureTerminalConfig();
  }, [ensureTerminalConfig]);

  // Settings that land after a terminal was created (or change while it is
  // live) are applied in place — changing the font must not rebuild a session.
  useEffect(() => {
    applyTerminalConfig(terminalConfig);
  }, [terminalConfig]);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const entry = acquireTerminal(sessionId, terminalConfig);
    entryRef.current = entry;
    mountTerminal(entry, host, terminalConfig.webgl);

    let disposed = false;
    // One snapshot at a time; a resync that lands mid-flight is coalesced.
    let snapshotting = false;
    let snapshotAgain = false;

    const snapshot = async (): Promise<void> => {
      if (snapshotting) {
        snapshotAgain = true;
        return;
      }
      snapshotting = true;
      try {
        do {
          snapshotAgain = false;
          const buffer = await cmd.sessionSnapshot(sessionId);
          if (disposed) return;
          const result = applySnapshot(entry, buffer);
          if (result.scrollbackLost) markScrollbackLost(sessionId);
        } while (snapshotAgain);
      } finally {
        snapshotting = false;
      }
    };

    const attach = async (): Promise<void> => {
      // 1. the channel, before anything is read
      const channel = new Channel<PtyChunk>();
      channel.onmessage = (chunk) => {
        const live = pooledTerminal(sessionId);
        if (live) applyChunk(live, chunk);
      };
      beginAttach(entry);
      await cmd.sessionAttach(sessionId, channel);
      if (disposed) return;
      entry.attached = true;
      // 2. and only then the ring
      await snapshot();
    };

    // A terminal that is already streaming keeps its channel: re-mounting is a
    // DOM move, not a new attach, so nothing is re-written and the scrollback
    // the user was reading stays exactly where it was.
    if (!entry.attached && !entry.closed) {
      void attach().catch((e) => {
        if (disposed) return;
        const error = asGildeError(e);
        setFailure(error.detail ?? error.key);
        if (!isUnavailable(e)) pushErrorToast(error);
      });
    }

    // DESIGN §6-4: BEL is the cheapest "the agent wants you" signal there is.
    const offBell = entry.term.onBell(() => setAttention(sessionId, true));
    const offData = entry.term.onData((data) => {
      void cmd.sessionWrite(sessionId, textToB64(data)).catch(() => undefined);
    });
    const offBinary = entry.term.onBinary((data) => {
      void cmd.sessionWrite(sessionId, latin1ToB64(data)).catch(() => undefined);
    });

    // §9-4: one immediate fit, because xterm's first measurement never matches
    // the 80×24 the pty was opened with.
    const fitAndResize = (force: boolean) => {
      if (disposed || !entry.container.isConnected) return;
      try {
        entry.fit.fit();
      } catch {
        return; // a zero-sized host during a layout change
      }
      const cols = entry.term.cols;
      const rows = entry.term.rows;
      if (cols <= 0 || rows <= 0) return;
      if (!force && entry.lastSize && entry.lastSize.cols === cols && entry.lastSize.rows === rows) {
        return;
      }
      entry.lastSize = { cols, rows };
      void cmd.sessionResize(sessionId, cols, rows).catch(() => undefined);
    };
    const initial = requestAnimationFrame(() => fitAndResize(true));

    const offResync = onGildeEvent(PTY_RESYNC, (event) => {
      if (event.sessionId !== sessionId) return;
      void snapshot();
    });

    return () => {
      disposed = true;
      cancelAnimationFrame(initial);
      offResync();
      offBell.dispose();
      offData.dispose();
      offBinary.dispose();
      releaseTerminal(sessionId);
      entryRef.current = null;
    };
    // `terminalConfig` is deliberately not a dependency: changing the font
    // must not tear down a live terminal. The effect above patches the live
    // ones instead.
  }, [sessionId]); // eslint-disable-line react-hooks/exhaustive-deps

  useResizeObserver(
    hostRef,
    () => {
      const entry = entryRef.current;
      if (!entry || !entry.container.isConnected) return;
      try {
        entry.fit.fit();
      } catch {
        return;
      }
      const cols = entry.term.cols;
      const rows = entry.term.rows;
      if (cols <= 0 || rows <= 0) return;
      if (entry.lastSize && entry.lastSize.cols === cols && entry.lastSize.rows === rows) return;
      entry.lastSize = { cols, rows };
      void cmd.sessionResize(sessionId, cols, rows).catch(() => undefined);
    },
    RESIZE_DEBOUNCE_MS,
  );

  useEffect(() => {
    if (!active) return;
    entryRef.current?.term.focus();
  }, [active]);

  return (
    <div
      className="term-host"
      ref={hostRef}
      // ⌘-hotkeys must work over xterm while every other key reaches the pty.
      data-gilde-hotkeys="allow"
    >
      {failure ? (
        <div className="term-failure s12" role="alert">
          {t('session.attachFailed')}
          <span className="mono s11 muted"> {failure}</span>
        </div>
      ) : null}
    </div>
  );
}

export default XTermHost;
