/**
 * The Session screen (ux-ia §3.4, §3.6, §3.13; contract §15.2 T15).
 *
 *   header   title · project › location · branch
 *            vendor · model · account · ownership · mode · memory · age   [Open in IDE ⌘O] [Add to split ⌘D] [⋯]
 *   tabs     Conversation · Changes n · Terminal | Log          (⌥⌘1/2/3, ⇧⌘T flips)
 *   body     the sub-view
 *   dock     the card stack (sticky above the composer), then the composer —
 *            or, without a composer, the ownership / continue bar; the error
 *            bar's primary action above it
 *
 * Rules: opening a session with a pending card (or arriving with `⌘J`)
 * focuses the card (`ui.focusCard`); a card arriving later never takes focus.
 * New reply becomes seen after 2 s on screen with the window visible
 * (`session_mark_seen`; a `⌘J` fly-through is shorter and never sends it).
 * The screen reports nothing itself: `app_view_state` follows the UI store.
 */

import { useCallback, useEffect, useId, useRef, useState, useSyncExternalStore } from 'react';

import { CardStack, ownerWord } from '../approval';
import { ChangesView, changesCountOf, subscribeChangesCount } from '../changes/ChangesView';
import { StateGlyph } from '../components/StateGlyph';
import { Composer } from '../composer/Composer';
import { ConversationView } from '../convo/ConversationView';
import { useHotkeys } from '../hooks/useHotkeys';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatBytes, formatDuration } from '../lib/format';
import type { Activity } from '../lib/gen/Activity';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { LogView } from '../terminal/LogView';
import { TerminalView } from '../terminal/TerminalView';
import { useNow } from '../approval/useNow';
import type { SessionSub } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { HistoryReadView } from '../history/HistoryReadView';
import { DetailsPopover } from './DetailsPopover';
import { ContinueBar, ErrorBar, OwnershipBar } from './SessionBars';
import { useStopOutcome } from './StopSessionSheet';
import { addToSplitOrAsk } from '../split/replace';
import './session.css';

/** New reply becomes Ready after this long on screen (ux-ia §4.6). */
export const SEEN_DWELL_MS = 2000;

export interface SessionScreenProps {
  uid: SessionUid;
  sub: SessionSub;
}

/** The state word of an activity (ux-ia §4.2, "likely" prefix for low confidence). */
export function stateWord(activity: Activity, t: ReturnType<typeof useI18n>['t']): string {
  const s = activity.state;
  let key: MessageKey;
  switch (s.kind) {
    case 'error':
      key =
        s.sub.kind === 'usageLimit'
          ? 'state.error.usageLimit'
          : s.sub.kind === 'signInNeeded'
            ? 'state.error.signIn'
            : s.sub.kind === 'didntStart'
              ? 'state.error.didntStart'
              : 'state.error.agent';
      break;
    case 'statusUnknown':
      key = 'state.unknown';
      break;
    default:
      key = `state.${s.kind}` as MessageKey;
  }
  const word = t(key);
  return activity.confidence === 'likely' ? t('state.likely', { state: word }) : word;
}

/** The third sub-view this session offers, if any (ux-ia §2.2). */
export function thirdView(session: Session): 'terminal' | 'log' | null {
  if (session.caps.terminalView && session.control.kind === 'inGilde' && session.control.pty) return 'terminal';
  if (session.caps.logView) return 'log';
  return null;
}

export function SessionScreen({ uid, sub }: SessionScreenProps) {
  const { t, td, locale } = useI18n();
  // The shell's `main` is the window's one main landmark, named after the
  // screen (ux-ia §10); this screen is a section named by its own heading.
  const titleId = useId();
  const session = useAppStore((s) => sessionOf(s.world, uid));
  const project = useAppStore((s) => (session?.projectId ? (s.world.projects.get(session.projectId) ?? null) : null));
  const location = useAppStore((s) => (session?.locationId ? (s.world.locations.get(session.locationId) ?? null) : null));
  const focusCard = useAppStore((s) => s.ui.focusCard);
  const visible = useAppStore((s) => s.ui.visibility.documentVisible);
  const setSessionSub = useAppStore((s) => s.setSessionSub);
  const setDraft = useAppStore((s) => s.setDraft);
  const now = useNow();
  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  const [token, setToken] = useState<number | null>(null);
  const openSheet = useAppStore((s) => s.openSheet);
  const sheetOpen = useAppStore((s) => s.ui.sheet !== null);
  const [line, setLine] = useState<{ key: string; params: Readonly<Record<string, unknown>>; detail?: string | null } | null>(null);
  const changesCount = useSyncExternalStore(subscribeChangesCount, () => (session ? changesCountOf(session.uid) : null));

  // ⌘J arrival or a notification click asked for the card; so does the
  // composer when several questions are unanswered. Each request bumps the
  // stack's focus token once; a card that merely arrives never does.
  const requestedToken = focusCard && session && focusCard.uid === session.uid ? focusCard.token : null;
  useEffect(() => {
    if (requestedToken !== null) setToken((n) => (n ?? 0) + 1);
  }, [requestedToken]);

  // New reply → seen after 2 s on screen (never while hidden).
  const newReply = session?.activity.state.kind === 'newReply';
  const resolved = session?.uid ?? null;
  useEffect(() => {
    if (!newReply || !visible || !resolved) return;
    const timer = setTimeout(() => void commands.session_mark_seen({ uid: resolved }).catch(() => undefined), SEEN_DWELL_MS);
    return () => clearTimeout(timer);
  }, [newReply, visible, resolved]);

  const focusComposer = useCallback(() => {
    if (sub !== 'conversation' && sub !== 'terminal') setSessionSub('conversation');
    requestAnimationFrame(() => composerRef.current?.focus());
  }, [setSessionSub, sub]);

  const third = session ? thirdView(session) : null;
  const pickSub = (next: SessionSub) => {
    if (next === 'terminal' && !third) return;
    setSessionSub(next);
  };

  const openIde = () => {
    if (!session) return;
    commands.ide_open({ target: { kind: 'folder', uid: session.uid } }).then(
      () => setLine(null),
      (e: IpcError) => setLine({ key: e.key, params: e.params, detail: e.detail }),
    );
  };

  useHotkeys(
    [
      { id: 'session.view.conversation', combo: 'mod+alt+1', run: () => pickSub('conversation') },
      { id: 'session.view.changes', combo: 'mod+alt+2', run: () => pickSub('changes') },
      { id: 'session.view.terminal', combo: 'mod+alt+3', run: () => pickSub('terminal') },
      { id: 'session.flipView', combo: 'mod+shift+t', when: () => third !== null, run: () => pickSub(sub === 'terminal' ? 'conversation' : 'terminal') },
      { id: 'ide.open', combo: 'mod+o', run: openIde },
      { id: 'split.add', combo: 'mod+d', run: () => session && addToSplitOrAsk(session.uid) },
      {
        id: 'stop',
        combo: 'mod+shift+backspace',
        when: () => !!session && session.caps.stopSession !== 'none',
        run: () => openSheet('stopSession'),
      },
    ],
    { scope: 'S', enabled: !!session && !sheetOpen },
  );

  // The stop sheet's outcome (the shell mounts the sheet, A23.5).
  const stopOutcome = useStopOutcome(session?.uid ?? null);
  useEffect(() => {
    if (!stopOutcome) return;
    setLine(stopOutcome.survivors > 0 ? { key: 'session.stopSurvivors', params: { n: stopOutcome.survivors } } : { key: 'session.stopped', params: {} });
  }, [stopOutcome?.seq]); // eslint-disable-line react-hooks/exhaustive-deps

  // A uid World does not hold: History's read-only view (A29.2, T18b grant);
  // the store's entity wins as soon as it appears.
  if (!session) return <HistoryReadView uid={uid} />;

  const agent = vendorBadge(session.vendor);
  const control = session.control;
  const watching = control.kind === 'watching';
  const notRunning = control.kind === 'notRunning';
  const canSend = session.caps.send !== 'none';
  const ownership =
    control.kind === 'inGilde'
      ? t('ownership.inGilde')
      : control.kind === 'watching'
        ? t('ownership.runsIn', { owner: ownerWord(session, t) })
        : null;
  const modeWord =
    control.kind === 'inGilde' && control.mode !== 'conversation' ? t(control.mode === 'terminal' ? 'mode.terminal' : 'mode.turnBased') : null;
  const meta = [
    agent,
    session.model,
    session.details.accountLabel,
    ownership,
    modeWord,
    stateWord(session.activity, t),
    session.memoryBytes !== null ? formatBytes(session.memoryBytes, locale) : null,
    session.startedAt ? formatDuration(Math.max(0, now - session.startedAt), locale) : null,
  ].filter((x): x is string => typeof x === 'string' && x !== '');
  const evidence = session.activity.evidence;

  const tabs: Array<{ sub: SessionSub; label: string }> = [
    { sub: 'conversation', label: t('session.tab.conversation') },
    { sub: 'changes', label: changesCount !== null && changesCount > 0 ? `${t('session.tab.changes')} ${changesCount}` : t('session.tab.changes') },
  ];
  if (third) tabs.push({ sub: 'terminal', label: third === 'terminal' ? t('session.tab.terminal') : t('session.tab.log') });
  const current: SessionSub = sub === 'terminal' && !third ? 'conversation' : sub;
  // A watched session's wait is told by the ownership bar (Go to {owner}).
  const showStack = !watching;

  const dock = (withComposer: boolean) => (
    <div className="g-session-dock">
      {showStack && (
        <CardStack uid={session.uid} shortcuts focusToken={token} onReply={focusComposer} onShowTerminal={() => pickSub('terminal')} />
      )}
      <ErrorBar session={session} onShowLog={() => pickSub('terminal')} />
      {watching && <OwnershipBar session={session} active />}
      {notRunning && <ContinueBar session={session} active />}
      {withComposer && canSend && !watching && !notRunning && (
        <Composer uid={session.uid} inputRef={composerRef} onFocusCard={() => setToken((n) => (n ?? 0) + 1)} />
      )}
    </div>
  );

  let body;
  switch (current) {
    case 'changes':
      body = (
        <ChangesView
          uid={session.uid}
          onAskCommit={(text) => {
            setDraft(session.uid, text);
            setSessionSub('conversation');
            requestAnimationFrame(() => composerRef.current?.focus());
          }}
        />
      );
      break;
    case 'terminal':
      body =
        third === 'terminal' && control.kind === 'inGilde' && control.pty ? (
          <>
            <TerminalView ptyId={control.pty} label={t('terminal.label', { agent })} active />
            {dock(false)}
          </>
        ) : (
          <>
            <LogView uid={session.uid} />
            {dock(true)}
          </>
        );
      break;
    default:
      body = (
        <>
          <ConversationView uid={session.uid} />
          {dock(true)}
        </>
      );
  }

  return (
    <section className="g-session" aria-labelledby={titleId} data-session={session.uid}>
      <div className="g-session-head">
        <div className="g-session-row">
          <StateGlyph state={session.activity.state.kind} likely={session.activity.confidence === 'likely'} />
          <h1 id={titleId} className="g-session-title">{session.title ?? agent}</h1>
          <span className="g-session-where">
            {[project?.name, location?.worktreeName ? `wt/${location.worktreeName}` : null, location?.branch ? location.branch : null]
              .filter(Boolean)
              .join(' › ')}
          </span>
        </div>
        <div className="g-session-row">
          <span className="g-session-meta">{meta.join(' · ')}</span>
          <span className="g-session-actions">
            <button type="button" className="g-sbtn" aria-keyshortcuts="Meta+O" onClick={openIde}>
              {t('action.openInIde')} <span className="g-key" aria-hidden="true">⌘O</span>
            </button>
            <button type="button" className="g-sbtn" aria-keyshortcuts="Meta+D" onClick={() => addToSplitOrAsk(session.uid)}>
              {t('session.addToSplit')} <span className="g-key" aria-hidden="true">⌘D</span>
            </button>
            {session.caps.stopSession !== 'none' && (
              <button type="button" className="g-sbtn" aria-keyshortcuts="Meta+Shift+Backspace" onClick={() => openSheet('stopSession')}>
                {t('action.stopSession')}
              </button>
            )}
            <DetailsPopover session={session} />
          </span>
        </div>
        {evidence && <p className="g-session-evidence">{td(evidence.key, evidence.params)}</p>}
        {line && (
          <p className="g-session-evidence" role="status">
            {td(line.key, line.params)}
            {line.detail ? <span className="mono"> {line.detail}</span> : null}
          </p>
        )}
        <div className="g-session-tabs" role="tablist" aria-label={t('session.subviews')}>
          {tabs.map((tab, i) => (
            <button
              key={tab.sub}
              type="button"
              role="tab"
              className="g-session-tab"
              aria-selected={current === tab.sub}
              aria-keyshortcuts={`Meta+Alt+${i + 1}`}
              tabIndex={current === tab.sub ? 0 : -1}
              onClick={() => pickSub(tab.sub)}
            >
              {tab.label}
            </button>
          ))}
        </div>
      </div>
      <div className="g-session-body" role="tabpanel" aria-label={tabs.find((x) => x.sub === current)?.label}>
        {body}
      </div>
    </section>
  );
}
