/**
 * The Log sub-view (ux-ia §2.2 "Log: raw events, mono, never stored").
 *
 * Conversation and turn-based sessions have no PTY; their third sub-view is
 * the raw event stream of this view's conversation subscription — one mono
 * line per channel event (seq, time, kind, the block it touches), kept in
 * memory only (the last 500) and gone when the view closes. The manifest has
 * no separate raw-frame channel, so the conversation channel is the source
 * (contract request filed for a frame-level log).
 *
 * The Log replaces the Conversation sub-view while it is shown, so the two
 * never hold two of the eight conversation subscriptions at once.
 */

import { useEffect, useMemo, useState } from 'react';

import { useI18n } from '../i18n/useI18n';
import { createChannel, disposeChannel } from '../lib/channel';
import { commands } from '../lib/commands';
import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { ConvoEvent } from '../lib/gen/ConvoEvent';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { SubId } from '../lib/gen/SubId';
import { newViewId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import './terminal.css';

export const LOG_LINES = 500;

interface LogLine {
  seq: number;
  at: number;
  text: string;
}

/** One event → one line of facts (kinds, ids, sizes): never the text itself. */
export function describeEvent(event: ConvoEvent): string {
  switch (event.kind) {
    case 'reset':
      return `reset ${event.reason} blocks=${event.blocks.length}${event.hasOlder ? ' hasOlder' : ''}`;
    case 'appended': {
      const k = event.block.kind;
      const extra =
        k.kind === 'toolCall'
          ? ` ${k.summary.tool} ${k.status}`
          : k.kind === 'interaction'
            ? ` ${k.interactionKind} ${k.outcome}`
            : k.kind === 'system'
              ? ` ${k.note.kind}`
              : k.kind === 'userText'
                ? ` ${k.origin}${k.delivery ? ` ${k.delivery}` : ''}`
                : '';
      return `appended ${event.block.id} ${k.kind}${extra}`;
    }
    case 'updated': {
      const p = event.patch;
      const extra =
        p.kind === 'textDelta'
          ? ` +${p.append.length}`
          : p.kind === 'toolResult'
            ? ` ${p.status}`
            : p.kind === 'interaction'
              ? ` ${p.outcome}`
              : ` ${p.block.kind.kind}`;
      return `updated ${event.blockId} ${p.kind}${extra}`;
    }
    case 'older':
      return `older blocks=${event.blocks.length}${event.hasOlder ? ' hasOlder' : ''}`;
    case 'source':
      return `source ${event.source}${event.note ? ` ${event.note.key}` : ''}`;
    case 'evicted':
      return 'evicted';
    case 'ended':
      return 'ended';
  }
}

export function LogView({ uid }: { uid: SessionUid }) {
  const { t, te, locale } = useI18n();
  const [lines, setLines] = useState<LogLine[]>([]);
  const [error, setError] = useState<IpcError | null>(null);

  useEffect(() => {
    let live = true;
    let subId: SubId | null = null;
    setLines([]);
    setError(null);
    const channel = createChannel<ConvoEnvelope>((envelope) => {
      if (!live) return;
      const line = { seq: envelope.seq, at: Date.now(), text: describeEvent(envelope.event) };
      setLines((prev) => (prev.length >= LOG_LINES ? [...prev.slice(prev.length - LOG_LINES + 1), line] : [...prev, line]));
    });
    commands.convo_subscribe({ uid, viewId: newViewId('log'), tailBlocks: 50 }, channel).then(
      (res) => {
        if (!live) void commands.convo_unsubscribe({ subId: res.subId }).catch(() => undefined);
        else subId = res.subId;
      },
      (e: IpcError) => {
        if (live) setError(e);
      },
    );
    return () => {
      live = false;
      disposeChannel(channel);
      if (subId) void commands.convo_unsubscribe({ subId }).catch(() => undefined);
    };
  }, [uid]);

  const time = useMemo(() => new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' }), [locale]);

  return (
    <section className="g-log" aria-label={t('log.label')}>
      <p className="g-log-note">{t('log.note')}</p>
      {error && <p className="g-log-note">{te(error)}</p>}
      {lines.length === 0 && !error ? (
        <p className="g-log-note">{t('log.empty')}</p>
      ) : (
        <ol className="g-log-list" role="log" aria-live="off" tabIndex={0}>
          {lines.map((l) => (
            <li key={l.seq}>
              <span className="g-log-seq">{l.seq}</span>
              <time>{time.format(new Date(l.at))}</time>
              <span>{l.text}</span>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
