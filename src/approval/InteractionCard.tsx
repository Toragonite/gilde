/**
 * One pending request as a card (ux-ia §3.5, contract §6.1.4, §6.2.4, §8.6).
 *
 * The card is the only place where a request is answered. Its rules:
 *  - digits answer only while the card has focus (scope C, `requireFocus`);
 *    they are never typed into a composer, and IME composition and key repeat
 *    never answer (`useHotkeys`);
 *  - answering keys **and** clicks are ignored for 700 ms after the card
 *    appears, becomes the top card, is focused by `⌘J`, or the window is
 *    activated (`useAnswerGuard`), visibly;
 *  - `default_to_no` cards take focus on Deny and keep `⌘⏎` inert;
 *  - `Esc` never denies: it leaves the card;
 *  - the card answers the exact `requestId` it shows, in the `answer_in`
 *    session for a mirrored card (F-15);
 *  - "Allow with a note" and "Deny and say why… ⇥" are choices, guarded like
 *    answers: they record a reply intent (kind + `answer_in` target) on the
 *    composer that receives focus, the host's (A44.3);
 *  - every detail line is an i18n key (vendor text = `card.detailText`, A19.7).
 *
 * The stack (`CardStack`) drives `⌘⏎`/`⌘⌫` through the handle this card
 * exposes, so the shortcuts pass the same guard as the buttons.
 */

import { useCallback, useEffect, useId, useImperativeHandle, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, ReactNode, RefObject } from 'react';

import { GuardNote } from '../components/GuardNote';
import { StateGlyph } from '../components/StateGlyph';
import type { GlyphKind } from '../components/StateGlyph';
import { useAnswerGuard } from '../hooks/useAnswerGuard';
import { isImeComposing, useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatDuration } from '../lib/format';
import type { AnswerVia } from '../lib/gen/AnswerVia';
import type { EvidenceLine } from '../lib/gen/EvidenceLine';
import type { InteractionDetailRes } from '../lib/gen/InteractionDetailRes';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { Session } from '../lib/gen/Session';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { answerErrorLine, sendDecision, sendScreenAction } from './answer';
import type { AnswerOutcome, AnswerResult } from './answer';
import {
  allowsNote,
  answerFromSelections,
  answerTarget,
  answerableQuestions,
  answersOnPick,
  currentQuestion,
  denyOption,
  denyWhyIntent,
  digitAction,
  digitOptions,
  isAllowRole,
  isScreen,
  isStructured,
  isWatched,
  opensTerminal,
  shortcutAction,
  toggleSelection,
  trustOption,
} from './decisions';
import type { CardAction, FreeTexts, Selections } from './decisions';
import { setReplyIntent } from './replyIntent';
import { useNow } from './useNow';
import './approval.css';

/** What the stack can ask of its top card. */
export interface CardHandle {
  /** `⌘⏎` / `⌘⌫` on this card (guarded like a click). */
  shortcut: (which: 'primary' | 'deny') => void;
  /** Move focus into the card (its default control). */
  focus: () => void;
}

export interface InteractionCardProps {
  /** The session whose pending list holds this card. */
  session: Session;
  pending: PendingInteraction;
  /** 0-based position in the stack, and the stack size. */
  index: number;
  count: number;
  variant: 'full' | 'row';
  /** Changes whenever the 700 ms guard must start again (new top card, `⌘J` arrival). */
  guardKey: string;
  handleRef?: RefObject<CardHandle | null>;
  onReply?: () => void;
  onLeave?: () => void;
  onShowTerminal?: () => void;
  onAnswered?: (requestId: PendingInteraction['requestId'], outcome: AnswerResult) => void;
  /** An answer came back `withdrawn` (the stack shows "Answered elsewhere."). */
  onWithdrawn?: (requestId: PendingInteraction['requestId']) => void;
}

type Line = { key: string; params: Readonly<Record<string, unknown>>; detail?: string | null } | null;

type DetailState = { kind: 'closed' } | { kind: 'loading' } | { kind: 'open'; res: InteractionDetailRes } | { kind: 'error'; error: IpcError };

const GLYPH: Readonly<Record<PendingInteraction['kind'], GlyphKind>> = {
  toolApproval: 'needsApproval',
  watchedApproval: 'needsApproval',
  screenPrompt: 'needsApproval',
  planApproval: 'needsAnswer',
  question: 'needsAnswer',
  watchedQuestion: 'needsAnswer',
  setupPrompt: 'needsAnswer',
};

/** The state word of a card's head. */
function headKey(pending: PendingInteraction): MessageKey {
  if (pending.kind === 'planApproval') return 'plan.ready';
  if (pending.kind === 'screenPrompt') {
    return pending.options.some((o) => isAllowRole(o.role)) ? 'state.needsApproval' : 'state.needsAnswer';
  }
  return GLYPH[pending.kind] === 'needsApproval' ? 'state.needsApproval' : 'state.needsAnswer';
}

function lastSegments(path: string | null | undefined, n = 2): string {
  if (!path) return '';
  return path.split('/').filter(Boolean).slice(-n).join('/');
}

type T = ReturnType<typeof useI18n>['t'];

/** The owner word of a watched card: the owning app's display name, else the origin word. */
export function ownerWord(session: Session, t: T): string {
  if (session.control.kind !== 'watching') return '';
  if (session.control.owner) return session.control.owner.appName;
  switch (session.control.origin) {
    case 'vsCode':
      return t('origin.vscode');
    case 'itermTeam':
      return t('origin.itermTeam');
    case 'background':
      return t('origin.background');
    case 'headless':
      return t('origin.headless');
    case 'orchestrator':
      return t('origin.orchestrator');
    case 'otherGilde':
      return t('origin.otherGilde');
    default:
      return t('origin.terminal');
  }
}

function KeyCap({ k }: { k: string }) {
  return (
    <span className="g-key" aria-hidden="true">
      {k}
    </span>
  );
}

export function InteractionCard(props: InteractionCardProps) {
  const { session, pending, index, count, variant, guardKey, handleRef, onReply, onLeave, onShowTerminal, onAnswered, onWithdrawn } = props;
  const { t, td, te, locale } = useI18n();
  const openSession = useAppStore((s) => s.openSession);
  const now = useNow();
  const rootRef = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const hintId = useId();
  const [selections, setSelections] = useState<Selections>({});
  const [freeTexts, setFreeTexts] = useState<FreeTexts>({});
  const [line, setLine] = useState<Line>(null);
  const [busy, setBusy] = useState(false);
  const [detail, setDetail] = useState<DetailState>({ kind: 'closed' });

  // A mirrored card (a teammate's request shown on its tile) answers in the
  // lead's session, with the lead's copy of the request (its fresh promptRev).
  const target = answerTarget(pending, session.uid);
  const answerInSession = useAppStore((s) => (pending.answerIn ? sessionOf(s.world, pending.answerIn) : null));
  const targetSession = answerInSession ?? session;
  const live = useMemo(
    () => (pending.answerIn ? (targetSession.pending.find((p) => p.requestId === pending.requestId) ?? pending) : pending),
    [pending, targetSession],
  );
  const mirrored = pending.answerIn !== null && pending.answerIn !== session.uid;

  const guard = useAnswerGuard(guardKey);
  const agent = vendorBadge(session.vendor);

  // Reset the per-request state when another request takes this slot.
  useEffect(() => {
    setSelections({});
    setFreeTexts({});
    setLine(null);
    setBusy(false);
    setDetail({ kind: 'closed' });
  }, [pending.requestId]);

  const canAnswer = useMemo(() => {
    const approve = targetSession.caps.approve;
    if (isStructured(live.kind)) return approve === 'structured';
    if (isScreen(live.kind)) return approve === 'screenKeys';
    return false;
  }, [live.kind, targetSession.caps.approve]);

  const showTerminal = useCallback(() => {
    if (onShowTerminal) onShowTerminal();
    else openSession(target, { sub: 'terminal' });
  }, [onShowTerminal, openSession, target]);

  const goToOwner = useCallback(() => {
    void commands
      .owner_focus({ uid: session.uid })
      .catch((error: IpcError) => setLine({ key: error.key, params: error.params, detail: error.detail }));
  }, [session.uid]);

  const settle = useCallback(
    (outcome: AnswerOutcome, thenTerminal: boolean) => {
      setBusy(false);
      if (outcome.ok) {
        if (outcome.result === 'withdrawn') onWithdrawn?.(pending.requestId);
        // The trust action stopped before Enter (A6): the terminal is the way on.
        if (outcome.result === 'aborted') setLine({ key: 'card.needsSetup', params: {} });
        if (thenTerminal && (outcome.result === 'sent' || outcome.result === 'answered')) showTerminal();
        onAnswered?.(pending.requestId, outcome.result);
        return;
      }
      if ('busy' in outcome) return;
      setLine({ ...answerErrorLine(outcome.error), detail: outcome.error.detail });
    },
    [onAnswered, onWithdrawn, pending.requestId, showTerminal],
  );

  const run = useCallback(
    (action: CardAction, via: AnswerVia) => {
      switch (action.kind) {
        case 'decide':
          if (!canAnswer) return;
          setBusy(true);
          setLine(null);
          void sendDecision(target, live, action.decision, via).then((o) => settle(o, false));
          return;
        case 'screen':
          if (!canAnswer) return;
          setBusy(true);
          setLine(null);
          void sendScreenAction(target, live, action.action).then((o) => settle(o, action.thenTerminal));
          return;
        case 'reply':
          // A44.3: the choice belongs to the composer that receives focus —
          // the host's (this stack's session) when the host gives `onReply`,
          // else the answering session's, which `openSession` shows — and
          // names where the answer goes (`answer_in` on a mirrored card).
          if (action.intent) {
            setReplyIntent(onReply ? session.uid : target, { requestId: live.requestId, kind: action.intent, target });
          }
          if (onReply) onReply();
          else openSession(target);
          return;
        case 'pick': {
          const q = live.questions[action.questionIndex];
          if (!q || !canAnswer) return;
          const next = toggleSelection(selections, action.questionIndex, action.label, q.multiSelect);
          setSelections(next);
          // A single-select pick replaces whatever was typed for that question.
          if (!q.multiSelect) setFreeTexts((f) => ({ ...f, [action.questionIndex]: '' }));
          if (answersOnPick(live)) {
            const decision = answerFromSelections(live, next);
            if (decision) {
              setBusy(true);
              setLine(null);
              void sendDecision(target, live, decision, via).then((o) => settle(o, false));
            }
          }
          return;
        }
        case 'focus':
          rootRef.current?.focus();
          return;
        case 'inert':
          setLine(action.line ? { key: action.line, params: {} } : null);
          return;
      }
    },
    [canAnswer, live, onReply, openSession, selections, session.uid, settle, target],
  );

  /** Every answering entry point goes through the guard (keys and clicks alike). */
  const guarded = guard.guard((action: CardAction, via: AnswerVia) => run(action, via));

  const submitSelections = () => {
    const decision = answerFromSelections(live, selections, freeTexts);
    if (decision) guarded({ kind: 'decide', decision }, 'button');
  };

  /** The card's "Other" field: a typed answer replaces a single-select question's pick. */
  const typeAnswer = (questionIndex: number, text: string) => {
    setFreeTexts((f) => ({ ...f, [questionIndex]: text }));
    if (text.trim() !== '' && live.questions[questionIndex]?.multiSelect === false) {
      setSelections((s) => ({ ...s, [questionIndex]: [] }));
    }
  };
  const typedAny = Object.values(freeTexts).some((text) => text.trim() !== '');

  const toggleDetail = useCallback(() => {
    if (detail.kind === 'open' || detail.kind === 'loading') {
      setDetail({ kind: 'closed' });
      return;
    }
    setDetail({ kind: 'loading' });
    commands.interaction_detail({ uid: target, requestId: live.requestId }).then(
      (res) => setDetail({ kind: 'open', res }),
      (error: IpcError) => setDetail({ kind: 'error', error }),
    );
  }, [detail.kind, live.requestId, target]);

  const defaultFocus = useCallback(() => {
    const root = rootRef.current;
    if (!root) return;
    if (live.defaultToNo) {
      const deny = root.querySelector<HTMLElement>('[data-role="deny"]');
      if (deny) {
        deny.focus();
        return;
      }
    }
    root.focus();
  }, [live.defaultToNo]);

  useImperativeHandle(
    handleRef,
    () => ({
      shortcut: (which) => {
        if (isWatched(live.kind)) {
          if (which === 'primary' && session.caps.ownerFocus) goToOwner();
          return;
        }
        guarded(shortcutAction(live, which), 'shortcut');
      },
      focus: defaultFocus,
    }),
    [defaultFocus, goToOwner, guarded, live, session.caps.ownerFocus],
  );

  // ---- keys while the card has focus (scope C) --------------------------
  const moveFocus = (dir: 1 | -1) => {
    const root = rootRef.current;
    if (!root) return;
    const controls = [...root.querySelectorAll<HTMLElement>('[data-card-control]')].filter(
      (el) => !(el as HTMLButtonElement).disabled,
    );
    if (controls.length === 0) return;
    const at = controls.indexOf(document.activeElement as HTMLElement);
    const next = at < 0 ? (dir === 1 ? 0 : controls.length - 1) : (at + dir + controls.length) % controls.length;
    controls[next]?.focus();
  };

  const bindings: HotkeyBinding[] = [];
  for (let d = 1; d <= 9; d++) {
    const digit = String(d);
    bindings.push({
      id: `card.digit.${digit}`,
      combo: digit,
      when: () => digitAction(live, digit, currentQuestion(live, selections, freeTexts)) !== null,
      run: () => {
        const action = digitAction(live, digit, currentQuestion(live, selections, freeTexts));
        if (action) guarded(action, 'digit');
      },
    });
  }
  bindings.push(
    {
      id: 'card.denyWhy',
      combo: 'tab',
      when: () => onReply !== undefined && live.kind !== 'setupPrompt',
      run: () => {
        // A deny with a reason is a choice: guarded, and recorded for the composer (A44.3).
        const intent = denyWhyIntent(live.kind);
        if (intent) guarded({ kind: 'reply', intent }, 'shortcut');
        else onReply?.();
      },
    },
    {
      id: 'card.leave',
      combo: 'escape',
      run: () => {
        if (onLeave) onLeave();
        else rootRef.current?.blur();
      },
    },
    {
      id: 'card.answer',
      combo: 'enter',
      when: (event) =>
        live.kind === 'question' && !(event.target instanceof HTMLButtonElement) && answerFromSelections(live, selections, freeTexts) !== null,
      run: () => submitSelections(),
    },
    {
      id: 'card.expand',
      combo: 'arrowright',
      when: (event) => event.target === rootRef.current && isStructured(live.kind) && live.kind !== 'question',
      run: () => toggleDetail(),
    },
    { id: 'card.next', combo: ['arrowdown', 'arrowright'], allowRepeat: true, run: () => moveFocus(1) },
    { id: 'card.previous', combo: ['arrowup', 'arrowleft'], allowRepeat: true, run: () => moveFocus(-1) },
  );

  useHotkeys(bindings, { scope: 'C', element: rootRef, enabled: !isWatched(live.kind) });

  // A held Enter/Space on a focused button would click again and again.
  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.repeat && (event.key === 'Enter' || event.key === ' ')) event.preventDefault();
  };

  // ---- rendering -----------------------------------------------------------
  const disabled = busy || !canAnswer;
  const btn = (
    key: string,
    label: ReactNode,
    action: CardAction,
    opts: { primary?: boolean; role?: string; shortcut?: string } = {},
  ) => (
    <button
      type="button"
      key={key}
      className={`g-cbtn${opts.primary ? ' g-cbtn-primary' : ''}`}
      data-card-control=""
      data-role={opts.role}
      aria-keyshortcuts={opts.shortcut}
      disabled={disabled}
      {...guard.props}
      onClick={() => guarded(action, 'button')}
    >
      {label}
    </button>
  );

  const denyWhy = denyWhyIntent(live.kind);
  const replyButton =
    variant === 'full' && onReply ? (
      <button
        type="button"
        className="g-cbtn g-cbtn-ghost"
        data-card-control=""
        data-action="denyWhy"
        aria-keyshortcuts="Tab"
        {...(denyWhy ? guard.props : {})}
        onClick={() => (denyWhy ? guarded({ kind: 'reply', intent: denyWhy }, 'button') : onReply())}
      >
        {t('approval.denyWhy')} <KeyCap k="⇥" />
      </button>
    ) : null;

  let body: ReactNode = null;
  let actions: ReactNode = null;

  if (isWatched(live.kind)) {
    const owner = ownerWord(session, t);
    body = (
      <p className="g-card-sub">
        {live.kind === 'watchedApproval' ? t('observed.waitingApproval', { owner }) : t('card.waitingAnswerIn', { owner })}
      </p>
    );
    actions = session.caps.ownerFocus ? (
      <button type="button" className="g-cbtn g-cbtn-primary" data-card-control="" aria-keyshortcuts="Meta+Enter" onClick={goToOwner}>
        {t('action.goToOwner', { owner })}
      </button>
    ) : null;
  } else if (live.kind === 'toolApproval') {
    actions = (
      <>
        {btn('1', <><KeyCap k="1" />{t('approval.allowOnce')}</>, { kind: 'decide', decision: { kind: 'allowOnce' } }, { primary: !live.defaultToNo, role: 'allowOnce', shortcut: '1' })}
        {live.alwaysLabel !== null &&
          btn('2', <><KeyCap k="2" />{t('approval.always', { prefix: live.alwaysLabel })}</>, { kind: 'decide', decision: { kind: 'allowSession' } }, { role: 'allowSession', shortcut: '2' })}
        {btn('3', <><KeyCap k="3" />{t('approval.deny')}</>, { kind: 'decide', decision: { kind: 'deny' } }, { role: 'deny', shortcut: '3' })}
        {replyButton}
      </>
    );
  } else if (live.kind === 'planApproval') {
    actions = (
      <>
        {btn('1', <><KeyCap k="1" />{t('plan.approve')}</>, { kind: 'decide', decision: { kind: 'approvePlan' } }, { primary: true, role: 'allowOnce', shortcut: '1' })}
        {btn('3', <><KeyCap k="3" />{t('plan.keepPlanning')}</>, { kind: 'reply' }, { role: 'deny', shortcut: '3' })}
      </>
    );
  } else if (live.kind === 'question') {
    body = (
      <QuestionList
        pending={live}
        selections={selections}
        freeTexts={freeTexts}
        disabled={disabled}
        guardProps={guard.props}
        onPick={(qi, label) => guarded({ kind: 'pick', questionIndex: qi, label }, 'button')}
        onType={typeAnswer}
        onSubmit={submitSelections}
      />
    );
    if (!answersOnPick(live) || typedAny) {
      actions = (
        <button
          type="button"
          className="g-cbtn g-cbtn-primary"
          data-card-control=""
          data-role="answer"
          aria-keyshortcuts="Enter"
          disabled={disabled || answerFromSelections(live, selections, freeTexts) === null}
          {...guard.props}
          onClick={submitSelections}
        >
          {t('card.answer')} <KeyCap k="⏎" />
        </button>
      );
    }
  } else if (live.kind === 'screenPrompt') {
    const digits = digitOptions(live);
    const deny = denyOption(live);
    const esc = live.options.find((o) => o.key === 'esc');
    body = (
      <div className="g-card-screen mono">
        {live.title !== '' ? <div>{live.title}</div> : <div className="g-card-muted">{t('prompt.questionNotShown')}</div>}
        {digits.map((o) => (
          <div key={o.key}>{`${o.key}. ${o.label}`}</div>
        ))}
      </div>
    );
    actions = (
      <>
        {digits.map((o) => {
          const isDeny = deny !== null && deny.key === o.key;
          return btn(
            o.key,
            <>
              <KeyCap k={o.key} />
              <span className="mono">{o.label}</span>
              {isDeny ? <span className="g-card-muted">{` ${t('screen.noStopsTurn')}`}</span> : null}
            </>,
            { kind: 'screen', action: { kind: 'key', key: o.key }, thenTerminal: opensTerminal(o) },
            { primary: isAllowRole(o.role) && !live.defaultToNo, role: isDeny ? 'deny' : o.role, shortcut: o.key },
          );
        })}
        {esc &&
          btn('esc', <><KeyCap k="Esc" /><span className="mono">{esc.label}</span></>, { kind: 'screen', action: { kind: 'key', key: 'esc' }, thenTerminal: false })}
        {replyButton}
        {variant === 'full' && onReply && allowsNote(live) && (
          <button
            type="button"
            className="g-cbtn g-cbtn-ghost"
            data-card-control=""
            data-action="allowWithNote"
            disabled={disabled}
            {...guard.props}
            onClick={() => guarded({ kind: 'reply', intent: 'allowWithNote' }, 'button')}
          >
            {t('approval.allowWithNote')}
          </button>
        )}
      </>
    );
  } else if (live.kind === 'setupPrompt') {
    const trust = trustOption(live);
    body = <p className="g-card-sub">{t('card.needsSetup')}</p>;
    actions = (
      <>
        {trust && btn('trust', t('card.trustContinue'), { kind: 'screen', action: { kind: 'trustFolder' }, thenTerminal: false }, { primary: true })}
        <button type="button" className={`g-cbtn${trust ? '' : ' g-cbtn-primary'}`} data-card-control="" onClick={showTerminal}>
          {t('card.continueInTerminal')}
        </button>
      </>
    );
  }

  // "Use the prompt's own keys." stays on a screen card whose shortcuts are
  // inert; the default_to_no line appears when ⌘⏎ is pressed (ux-ia §3.5).
  const hint = live.kind === 'screenPrompt' && live.defaultToNo ? t('approval.useOwnKeys') : null;
  const lineText = line ? td(line.key, line.params) : null;

  return (
    <div
      ref={rootRef}
      className="g-card"
      data-variant={variant}
      data-kind={live.kind}
      data-request-id={live.requestId}
      data-default-no={live.defaultToNo ? 'true' : undefined}
      role="group"
      tabIndex={0}
      aria-labelledby={titleId}
      aria-describedby={hint ? hintId : undefined}
      aria-busy={busy || undefined}
      onKeyDown={onKeyDown}
    >
      <div className="g-card-head">
        <StateGlyph state={GLYPH[live.kind]} likely={live.confidence === 'likely'} />
        <span className="g-card-state">
          {live.confidence === 'likely' ? t('state.likely', { state: t(headKey(live)) }) : t(headKey(live))}
        </span>
        {mirrored && <span className="g-card-note">{t('team.answeredAtLead')}</span>}
        {count > 1 && <span className="g-card-pos">{t('card.stackPosition', { i: index + 1, n: count })}</span>}
        <span className="g-card-age mono">
          {t('card.waiting', { age: formatDuration(Math.max(0, now - live.receivedAt), locale) })}
        </span>
      </div>
      <p className="g-card-title" id={titleId}>
        {cardTitle(live, session, agent, t)}
      </p>
      <DetailLines lines={live.detailLines} td={td} />
      {body}
      {detail.kind !== 'closed' && <DetailPanel state={detail} te={te} t={t} />}
      <div className="g-card-actions">
        {actions}
        {isStructured(live.kind) && live.kind !== 'question' && variant === 'full' && (
          <button
            type="button"
            className="g-cbtn g-cbtn-ghost g-card-expand"
            data-card-control=""
            aria-expanded={detail.kind === 'open'}
            onClick={toggleDetail}
          >
            {detail.kind === 'open' ? t('card.collapse') : t('card.expand')}
          </button>
        )}
        <GuardNote guard={guard} />
      </div>
      {hint && (
        <p className="g-card-hint" id={hintId}>
          {hint}
        </p>
      )}
      {lineText && lineText !== hint && (
        <p className="g-card-line" data-card-line="">
          {lineText}
          {line?.detail ? <span className="g-card-detail mono">{line.detail}</span> : null}
        </p>
      )}
    </div>
  );
}

/** The card's title: the request's own title, else the generic sentence per verb (ux-ia #73/#74). */
export function cardTitle(pending: PendingInteraction, session: Session, agent: string, t: T): string {
  if (pending.kind === 'planApproval') return pending.title !== '' ? pending.title : t('plan.ready');
  if (pending.kind === 'setupPrompt' || pending.kind === 'screenPrompt') {
    return pending.title !== '' ? pending.title : t('prompt.questionNotShown');
  }
  if (pending.kind === 'question' || pending.kind === 'watchedQuestion') {
    if (pending.title !== '') return pending.title;
    return pending.questions[0]?.text ?? t('prompt.questionNotShown');
  }
  if (pending.degraded) return t('card.degraded', { agent });
  if (pending.title !== '') return pending.title;
  const tool = pending.tool;
  const dir = lastSegments(session.details.location ?? session.details.cwd ?? session.cwd) || '~';
  switch (tool?.verb) {
    case 'run':
      return t('approval.command', { agent, dir });
    case 'edit':
    case 'create':
      return t('approval.edit', { agent, file: tool.object ?? '' });
    case 'reach':
      return t('approval.reach', { agent, host: tool.object ?? '' });
    default:
      return t('approval.use', { agent, tool: tool?.tool ?? t('compat.unreadableRequest') });
  }
}

function DetailLines({ lines, td }: { lines: readonly EvidenceLine[]; td: ReturnType<typeof useI18n>['td'] }) {
  if (lines.length === 0) return null;
  return (
    <div className="g-card-details">
      {lines.map((line, i) =>
        line.key === 'card.detailText' ? (
          <pre className="g-card-cmd" key={i}>
            {td(line.key, line.params)}
          </pre>
        ) : (
          <p className="g-card-sub" key={i}>
            {td(line.key, line.params)}
          </p>
        ),
      )}
    </div>
  );
}

function DetailPanel({ state, te, t }: { state: DetailState; te: ReturnType<typeof useI18n>['te']; t: T }) {
  if (state.kind === 'loading') return <p className="g-card-sub">{t('common.loading')}</p>;
  if (state.kind === 'error') return <p className="g-card-line">{te(state.error)}</p>;
  if (state.kind !== 'open') return null;
  return (
    <div className="g-card-full">
      <div className="g-card-sub mono">{state.res.toolName}</div>
      <pre className="g-card-cmd">{state.res.inputPretty}</pre>
      {state.res.decisionReason ? <p className="g-card-sub">{state.res.decisionReason}</p> : null}
    </div>
  );
}

interface QuestionListProps {
  pending: PendingInteraction;
  selections: Selections;
  freeTexts: FreeTexts;
  disabled: boolean;
  guardProps: Record<string, unknown>;
  onPick: (questionIndex: number, label: string) => void;
  onType: (questionIndex: number, text: string) => void;
  /** `⏎` in an "Other" field (keys in a text field never reach the card's bindings). */
  onSubmit: () => void;
}

/** What an option says besides its label: its description, then its preview. */
function OptionDetail({ description, preview }: { description: string | null | undefined; preview: string | null | undefined }) {
  return (
    <>
      {description ? <span className="g-card-odesc">{description}</span> : null}
      {preview ? <pre className="g-card-opreview mono">{preview}</pre> : null}
    </>
  );
}

function QuestionList({ pending, selections, freeTexts, disabled, guardProps, onPick, onType, onSubmit }: QuestionListProps) {
  const { t } = useI18n();
  const answerable = new Set(answerableQuestions(pending));
  const current = currentQuestion(pending, selections, freeTexts);
  return (
    <div className="g-card-questions">
      {pending.questions.map((q, qi) => {
        const open = answerable.has(qi);
        const picked = selections[qi] ?? [];
        return (
          <fieldset key={qi} className="g-card-question" data-current={qi === current ? 'true' : undefined} disabled={!open}>
            {/* The card's title already shows a lone question: keep it for screen readers only. */}
            <legend className={pending.questions.length === 1 && q.text === pending.title ? 'g-sr-only' : 'g-card-qtext'}>{q.text}</legend>
            {q.header ? <span className="g-card-qheader badge">{q.header}</span> : null}
            {open && (
              <div className="g-card-options">
                {q.options.map((label, oi) => {
                  const digit = oi < 9 && qi === current ? String(oi + 1) : null;
                  const selected = picked.includes(label);
                  return q.multiSelect ? (
                    <label key={oi} className="g-cbtn g-card-check" data-selected={selected ? 'true' : undefined}>
                      <input
                        type="checkbox"
                        data-card-control=""
                        checked={selected}
                        disabled={disabled}
                        {...guardProps}
                        onChange={() => onPick(qi, label)}
                      />
                      {digit ? <KeyCap k={digit} /> : null}
                      <span className="g-card-olabel">
                        <span>{label}</span>
                        <OptionDetail description={q.descriptions[oi]} preview={q.previews[oi]} />
                      </span>
                    </label>
                  ) : (
                    <button
                      type="button"
                      key={oi}
                      className="g-cbtn"
                      data-card-control=""
                      data-role="answer"
                      aria-pressed={selected}
                      aria-keyshortcuts={digit ?? undefined}
                      disabled={disabled}
                      {...guardProps}
                      onClick={() => onPick(qi, label)}
                    >
                      {digit ? <KeyCap k={digit} /> : null}
                      <span className="g-card-olabel">
                        <span>{label}</span>
                        <OptionDetail description={q.descriptions[oi]} preview={q.previews[oi]} />
                      </span>
                    </button>
                  );
                })}
              </div>
            )}
            {open && (
              <input
                type="text"
                className="g-card-other"
                data-card-control=""
                placeholder={t('card.otherPlaceholder')}
                aria-label={t('card.otherLabel', { index: qi + 1 })}
                value={freeTexts[qi] ?? ''}
                maxLength={2000}
                disabled={disabled}
                onChange={(event) => onType(qi, event.target.value)}
                onKeyDown={(event) => {
                  if (event.key !== 'Enter' || isImeComposing(event.nativeEvent)) return;
                  event.preventDefault();
                  if (!event.repeat) onSubmit();
                }}
              />
            )}
          </fieldset>
        );
      })}
    </div>
  );
}
