/**
 * The session composer (ux-ia §3.4, §3.5, §4.7, §4.8; contract §6.1.3, §8.6).
 *
 *  - `⏎` sends, `⇧⏎` is a newline, `⌥⏎` sends now (Conversation: the CLI
 *    reads it in the same turn or restarts the reply — "Send now may restart
 *    the reply."). IME composition never sends; a held `⏎` sends once.
 *  - While a turn runs a message is queued; queued messages show as chips
 *    with Edit and Cancel (`turn_cancel_queued`).
 *  - While a card is pending, typing a reply answers it — but only when the
 *    card had been on screen ≥ 700 ms before the draft began (F-38); then the
 *    hint names what `⏎` will do per card kind (F-11, N-12). Otherwise `⏎`
 *    sends an ordinary message and the card stays.
 *  - `⌘.` stops the turn: "Stopping…", then after 5 s "Still stopping…" with
 *    Force stop (ux-ia §4.8). "Deny and stop" (no message field, R6 LP-03)
 *    is offered next to Send while a tool approval is armed.
 *  - Chips: model (`⇧⌘M`, from the next message), permission mode (what
 *    `system/status` reported, `Session.permissionMode`), account.
 *
 * The draft lives in the UI store per session (memory only).
 */

import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react';
import type {
  ChangeEvent,
  ClipboardEvent as ReactClipboardEvent,
  DragEvent as ReactDragEvent,
  KeyboardEvent as ReactKeyboardEvent,
  RefObject,
} from 'react';

import { answerTarget, sendDecision, setReplyIntent, useReplyIntent } from '../approval';
import { ARM_MS } from '../approval/decisions';
import { useHotkeys } from '../hooks/useHotkeys';
import { isImeComposing } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import type { ClientMsgId } from '../lib/gen/ClientMsgId';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { SendPriority } from '../lib/gen/SendPriority';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { attachmentOf, imageFiles, MAX_IMAGES, SEND_TYPES } from './attachments';
import type { Attachment } from './attachments';
import { ComposerChips } from './ComposerChips';
import { composerIntent, intentHintKey, stopTurn, submit } from './send';
import './composer.css';

/** "Still stopping…" after this long without the turn ending (ux-ia §4.8). */
export const STILL_STOPPING_MS = 5000;

export interface ComposerProps {
  uid: SessionUid;
  /** The host focuses this for "Deny and say why…" and Ready's "Message". */
  inputRef?: RefObject<HTMLTextAreaElement | null>;
  /** The S-scope keys (`⌘.`, `⇧⌘M`) are live (the focused session or tile). Default true. */
  active?: boolean;
  /** Several unanswered questions: `⏎` moves focus to the card instead of sending. */
  onFocusCard?: () => void;
  /** Tiles: one row of chips, no queue details. */
  compact?: boolean;
}

/** `queued`: the line is "Queued…" for this message and holds only while it waits. */
type Line = { key: string; params: Readonly<Record<string, unknown>>; detail?: string | null; queued?: ClientMsgId } | null;

type StopState = { phase: 'stopping'; at: number } | { phase: 'still'; at: number } | { phase: 'stopped' } | null;

const EMPTY: readonly PendingInteraction[] = [];

/** One composer per session: its arming and stop state never carry over to another session. */
export function Composer(props: ComposerProps) {
  return <ComposerBody key={props.uid} {...props} />;
}

function ComposerBody({ uid, inputRef, active = true, onFocusCard, compact = false }: ComposerProps) {
  const { t, td } = useI18n();
  const session = useAppStore((s) => sessionOf(s.world, uid));
  const draft = useAppStore((s) => (session ? (s.ui.drafts[session.uid] ?? '') : ''));
  const setDraft = useAppStore((s) => s.setDraft);
  const clearDraft = useAppStore((s) => s.clearDraft);
  const ownRef = useRef<HTMLTextAreaElement | null>(null);
  const ref = inputRef ?? ownRef;
  const hintId = useId();
  const [line, setLine] = useState<Line>(null);
  const [sending, setSending] = useState(false);
  const [stop, setStop] = useState<StopState>(null);
  const [attachments, setAttachments] = useState<readonly Attachment[]>([]);
  const fileRef = useRef<HTMLInputElement | null>(null);
  const [, setTick] = useState(0);

  const pending = session?.pending ?? EMPTY;
  const top = pending[0] ?? null;
  // The intent a card recorded on this composer — the host of the card (A44.3).
  const intentForSession = useReplyIntent(session?.uid ?? null);
  // A mirrored card answers in its `answer_in` session, with that session's
  // copy of the request (its fresh promptRev), as the card itself does (F-15).
  const answerUid = top && session ? answerTarget(top, session.uid) : null;
  const live = useAppStore((s) =>
    top && answerUid && answerUid !== session?.uid ? (sessionOf(s.world, answerUid)?.pending.find((p) => p.requestId === top.requestId) ?? null) : null,
  );

  // An intent ends with its request: once the request left this composer's
  // cards, the next card starts from the composer's own mapping again.
  useEffect(() => {
    if (!session || !intentForSession) return;
    if (!pending.some((p) => p.requestId === intentForSession.requestId)) setReplyIntent(session.uid, null);
  }, [intentForSession, pending, session]);

  // When this composer first saw the current top card, and when the current
  // draft began (F-38). A draft that already existed counts as begun before.
  const shown = useRef<{ requestId: string | null; at: number | null }>({ requestId: null, at: null });
  if ((top?.requestId ?? null) !== shown.current.requestId) {
    shown.current = { requestId: top?.requestId ?? null, at: top ? clock.now() : null };
  }
  const draftStart = useRef<number | null>(draft !== '' ? Number.NEGATIVE_INFINITY : null);

  // Re-render when the card becomes armed, so the hint appears by itself.
  useEffect(() => {
    if (!top) return;
    const timer = setTimeout(() => setTick((n) => n + 1), ARM_MS + 5);
    return () => clearTimeout(timer);
  }, [top?.requestId]); // eslint-disable-line react-hooks/exhaustive-deps

  const intent = session
    ? composerIntent(session.uid, top, intentForSession, shown.current.at, draftStart.current, clock.now(), live ?? top)
    : ({ kind: 'send' } as const);
  const hintKey = intentHintKey(intent);
  const agent = session ? vendorBadge(session.vendor) : '';
  const conversation = session?.control.kind === 'inGilde' && session.control.mode === 'conversation';
  const turnBased = session?.control.kind === 'inGilde' && session.control.mode === 'turnBased';
  const working = session?.activity.state.kind === 'working' || session?.activity.state.kind === 'noProgress';

  // Stop feedback (ux-ia §4.8): the turn ending clears it; 5 s without that → "Still stopping…".
  useEffect(() => {
    if (!stop || stop.phase === 'stopped') return;
    if (!working && pending.length === 0) {
      setStop(null);
      return;
    }
    if (stop.phase === 'stopping') {
      // Checked against the UI clock in short steps (only while stopping), so
      // the 5 s rule follows `lib/clock` like the other timing rules.
      const timer = setInterval(() => {
        if (clock.now() - stop.at >= STILL_STOPPING_MS) setStop((s) => (s && s.phase === 'stopping' ? { phase: 'still', at: s.at } : s));
      }, 250);
      return () => clearInterval(timer);
    }
    return undefined;
  }, [stop, working, pending.length]);

  useEffect(() => {
    if (stop?.phase !== 'stopped') return;
    const timer = setTimeout(() => setStop(null), 6000);
    return () => clearTimeout(timer);
  }, [stop]);

  const onChange = (event: ChangeEvent<HTMLTextAreaElement>) => {
    if (!session) return;
    const value = event.target.value;
    if (value === '') draftStart.current = null;
    else if (draft === '' || draftStart.current === null) draftStart.current = clock.now();
    setDraft(session.uid, value);
    if (line) setLine(null);
  };

  const send = useCallback(
    async (priority: SendPriority) => {
      if (!session || sending) return;
      const now = clock.now();
      const current = composerIntent(session.uid, top, intentForSession, shown.current.at, draftStart.current, now, live ?? top);
      if (current.kind === 'send' && session.caps.send === 'none') return;
      const images = current.kind === 'send' ? attachments : [];
      if (draft.trim() === '' && images.length === 0 && current.kind !== 'replyInstead') return;
      setSending(true);
      const target = current.kind === 'send' || !top ? session.uid : answerTarget(top, session.uid);
      const result = await submit(target, current, draft, priority, images);
      setSending(false);
      switch (result.kind) {
        case 'done':
          if (images.length > 0) setAttachments([]);
          clearDraft(session.uid);
          draftStart.current = null;
          setReplyIntent(session.uid, null);
          setLine(result.queued ? { key: 'composer.queued', params: { agent }, queued: result.queued } : null);
          break;
        case 'kept':
          setLine(result.line.key === '' ? null : { ...result.line, detail: result.error?.detail ?? null });
          break;
        case 'focusCard':
          onFocusCard?.();
          break;
        case 'busy':
          break;
      }
    },
    [agent, attachments, clearDraft, draft, intentForSession, live, onFocusCard, sending, session, top],
  );

  // Images: pasted, dropped or picked; a Conversation session takes them.
  const addImages = async (files: readonly File[]) => {
    if (files.length === 0) return;
    if (!conversation) {
      setLine({ key: 'composer.imagesConversationOnly', params: { agent } });
      return;
    }
    const room = MAX_IMAGES - attachments.length;
    const read = await Promise.all(files.slice(0, Math.max(0, room)).map((f) => attachmentOf(f).catch(() => null)));
    const added = read.filter((a): a is Attachment => a !== null);
    if (added.length > 0) setAttachments((list) => [...list, ...added].slice(0, MAX_IMAGES));
    if (files.length > room) setLine({ key: 'composer.imagesMax', params: { n: MAX_IMAGES } });
    else if (added.length < files.length) setLine({ key: 'composer.imageUnreadable', params: {} });
  };

  const onPaste = (event: ReactClipboardEvent<HTMLTextAreaElement>) => {
    const files = imageFiles(event.clipboardData);
    if (files.length === 0) return;
    // A screenshot pastes as an image only; text that came with it still types.
    if (!event.clipboardData.types.includes('text/plain')) event.preventDefault();
    void addImages(files);
  };

  const onDrop = (event: ReactDragEvent<HTMLDivElement>) => {
    const files = imageFiles(event.dataTransfer);
    if (files.length === 0) return;
    event.preventDefault();
    void addImages(files);
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== 'Enter') return;
    // A Korean syllable is finished with ⏎: never a send (T-ui-ime).
    if (isImeComposing(event.nativeEvent)) return;
    if (event.shiftKey || event.metaKey || event.ctrlKey) return;
    event.preventDefault();
    if (event.repeat) return;
    void send(event.altKey && conversation ? 'now' : 'next');
  };

  const stopNow = useCallback(
    async (discardQueued: boolean) => {
      if (!session || !session.caps.interrupt) return;
      try {
        const res = await stopTurn(session.uid, session.pending, discardQueued);
        if (res.outcome === 'interrupting') setStop({ phase: 'stopping', at: clock.now() });
        else if (res.outcome === 'stopped') setStop({ phase: 'stopped' });
      } catch (e) {
        const error = e as IpcError;
        setLine({ key: error.key, params: error.params, detail: error.detail });
      }
    },
    [session],
  );

  const forceStop = useCallback(async () => {
    if (!session) return;
    try {
      await commands.turn_force_stop({ uid: session.uid });
    } catch (e) {
      const error = e as IpcError;
      setLine({ key: error.key, params: error.params, detail: error.detail });
    }
  }, [session]);

  const denyAndStop = useCallback(async () => {
    if (!session || !top || top.kind !== 'toolApproval') return;
    const outcome = await sendDecision(answerTarget(top, session.uid), top, { kind: 'denyAndStop' }, 'button');
    if (!outcome.ok && !('busy' in outcome)) setLine({ key: outcome.error.key, params: outcome.error.params, detail: outcome.error.detail });
  }, [session, top]);

  useHotkeys(
    [
      {
        id: 'turn.stop',
        combo: 'mod+period',
        when: () => !!session?.caps.interrupt,
        run: () => void stopNow(false),
      },
    ],
    { scope: 'S', enabled: active && !!session },
  );

  const queued = session?.queued ?? [];
  const cancelQueued = useCallback(
    async (clientMsgId: Session['queued'][number]['clientMsgId'], edit: boolean) => {
      if (!session) return;
      const message = session.queued.find((q) => q.clientMsgId === clientMsgId);
      try {
        const res = await commands.turn_cancel_queued({ uid: session.uid, clientMsgId });
        if (edit && res.cancelled && message) {
          setDraft(session.uid, draft.trim() === '' ? message.text : `${message.text}\n${draft}`);
          draftStart.current = clock.now();
          ref.current?.focus();
        }
      } catch (e) {
        const error = e as IpcError;
        setLine({ key: error.key, params: error.params, detail: error.detail });
      }
    },
    [draft, ref, session, setDraft],
  );

  const stopLine = useMemo(() => {
    if (!stop) return null;
    if (stop.phase === 'stopping') return t('composer.stopping');
    if (stop.phase === 'still') return t('composer.stillStopping');
    return t('turn.stopped');
  }, [stop, t]);

  if (!session) return null;
  const canSend = session.caps.send !== 'none';
  const armedDeny = intent.kind === 'denyWithMessage';
  // "Queued…" goes with its message: once the agent read it (or merged it into
  // the turn, or it was cancelled) the line is gone, not at the next keystroke.
  const shownLine = line?.queued && !queued.some((q) => q.clientMsgId === line.queued) ? null : line;

  return (
    <div
      className="g-composer"
      data-compact={compact ? 'true' : undefined}
      data-intent={intent.kind}
      onDragOver={(event) => {
        if (Array.from(event.dataTransfer.types).includes('Files')) event.preventDefault();
      }}
      onDrop={onDrop}
    >
      {hintKey && (
        <p className="g-composer-hint" id={hintId} data-composer-hint="">
          {t(hintKey, { agent })}
        </p>
      )}
      {turnBased && working && <p className="g-composer-hint">{t('turn.running')}</p>}
      <textarea
        ref={ref}
        className="g-composer-input"
        aria-label={t('composer.placeholder', { agent })}
        aria-describedby={hintKey ? hintId : undefined}
        placeholder={t('composer.placeholder', { agent })}
        value={draft}
        rows={compact ? 1 : 2}
        disabled={!canSend && !top}
        onChange={onChange}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
      />
      {attachments.length > 0 && (
        <ul className="g-composer-images" aria-label={t('composer.images', { n: attachments.length })}>
          {attachments.map((a, i) => (
            <li key={a.key} className="g-composer-image">
              <img src={a.url} alt={t('convo.image', { index: i + 1 })} />
              <button
                type="button"
                className="g-cbtn g-cbtn-ghost"
                aria-label={t('composer.removeImage', { index: i + 1 })}
                onClick={() => setAttachments((list) => list.filter((x) => x.key !== a.key))}
              >
                {t('composer.remove')}
              </button>
            </li>
          ))}
        </ul>
      )}
      {shownLine && shownLine.key !== '' && (
        <p className="g-composer-line" role="status">
          {td(shownLine.key, shownLine.params)}
          {shownLine.detail ? <span className="g-composer-detail mono">{shownLine.detail}</span> : null}
        </p>
      )}
      {!compact && queued.length > 0 && (
        <ul className="g-composer-queue" aria-label={t('composer.queuedChip', { n: queued.length })}>
          {queued.map((q) => (
            <li key={q.clientMsgId} className="g-composer-queued">
              <span className="g-composer-chip">{t('convo.delivery.queued')}</span>
              <span className="g-composer-qtext">{q.text}</span>
              <button type="button" className="g-cbtn g-cbtn-ghost" onClick={() => void cancelQueued(q.clientMsgId, true)}>
                {t('composer.edit')}
              </button>
              <button type="button" className="g-cbtn g-cbtn-ghost" onClick={() => void cancelQueued(q.clientMsgId, false)}>
                {t('common.cancel')}
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="g-composer-bar">
        <ComposerChips session={session} active={active} compact={compact} />
        {conversation && !compact && (
          <>
            <input
              ref={fileRef}
              type="file"
              accept={SEND_TYPES.join(',')}
              multiple
              hidden
              aria-label={t('composer.attachImage')}
              onChange={(event) => {
                void addImages(Array.from(event.target.files ?? []));
                event.target.value = '';
              }}
            />
            <button
              type="button"
              className="g-cbtn g-cbtn-ghost"
              disabled={attachments.length >= MAX_IMAGES || !canSend}
              onClick={() => fileRef.current?.click()}
            >
              {t('composer.attachImage')}
            </button>
          </>
        )}
        {compact && queued.length > 0 && <span className="g-composer-chip">{t('composer.queuedChip', { n: queued.length })}</span>}
        <span className="g-composer-spacer" />
        {stopLine && <span className="g-composer-stop">{stopLine}</span>}
        {stop?.phase === 'still' && session.caps.forceStop && (
          <button type="button" className="g-cbtn" onClick={() => void forceStop()}>
            {t('action.forceStop')}
          </button>
        )}
        {conversation && working && canSend && !compact && (
          <span className="g-composer-sendnow" title={t('composer.sendNowRestarts')}>
            {t('composer.sendNowHint')}
          </span>
        )}
        {session.caps.interrupt && (working || pending.length > 0) && !stop && (
          <>
            {queued.length > 0 && !compact && (
              <button type="button" className="g-cbtn g-cbtn-ghost" onClick={() => void stopNow(true)}>
                {t('action.stopDiscardQueued')}
              </button>
            )}
            <button type="button" className="g-cbtn" aria-keyshortcuts="Meta+." onClick={() => void stopNow(false)}>
              {t('action.stop')} <span className="g-key" aria-hidden="true">⌘.</span>
            </button>
          </>
        )}
        {armedDeny && !compact && (
          <button type="button" className="g-cbtn" onClick={() => void denyAndStop()}>
            {t('composer.denyAndStop')}
          </button>
        )}
        <button
          type="button"
          className="g-cbtn g-cbtn-primary"
          aria-keyshortcuts="Enter"
          disabled={sending || (draft.trim() === '' && (attachments.length === 0 || intent.kind !== 'send')) || (!canSend && intent.kind === 'send')}
          onClick={() => void send('next')}
        >
          {t('action.send')}{' '}
          <span className="g-key" aria-hidden="true">⏎</span>
        </button>
      </div>
      {conversation && working && canSend && !compact && draft !== '' && (
        <p className="g-composer-note">{t('composer.sendNowRestarts')}</p>
      )}
    </div>
  );
}
