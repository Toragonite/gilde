/**
 * What sits in the composer's place when there is no composer, or above it
 * when the session needs an action (ux-ia §2.3, §3.6, §4.3):
 *
 *  - `OwnershipBar` (Watching): one sentence, at most two actions — the
 *    primary is also `⌘⏎` — and "Why can't I answer here?";
 *  - `ContinueBar` (Not running): "This session ended at {time}. You can
 *    continue it here." with Continue in Gilde, or "can't be continued" with
 *    Open folder in IDE;
 *  - `ErrorBar` (In Gilde, Error): the state's primary action — Continue on
 *    another account, Sign in…, Retry, and for a Conversation start that
 *    failed on this claude version, Start in Terminal mode
 *    (`session_retry_as_terminal`).
 *
 * Every action is offered iff the session's caps allow it (ux-ia P4).
 */

import { useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import { ownerWord } from '../approval';
import { Popover, popoverTriggerProps } from '../components/Popover';
import { useHotkeys } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatTimeOfDay } from '../lib/format';
import type { ContinueHow } from '../lib/gen/ContinueHow';
import type { Session } from '../lib/gen/Session';
import { newClientMsgId, newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { useAppStore } from '../store/useAppStore';

type ErrLine = IpcError | null;

function useContinue(session: Session) {
  const openSession = useAppStore((s) => s.openSession);
  const [error, setError] = useState<ErrLine>(null);
  const [busy, setBusy] = useState(false);
  const run = (how: ContinueHow) => {
    if (busy) return;
    setBusy(true);
    commands
      .session_continue({ clientRequestId: newClientRequestId(), uid: session.uid, how, controlMode: null, model: null, permissionMode: null })
      .then(
        (res) => {
          setBusy(false);
          setError(null);
          if (res.uid !== session.uid) openSession(res.uid);
        },
        (e: IpcError) => {
          setBusy(false);
          setError(e);
        },
      );
  };
  return { run, error, busy };
}

function ErrorText({ error }: { error: ErrLine }) {
  const { te } = useI18n();
  if (!error) return null;
  return (
    <p className="g-bar-line" role="status">
      {te(error)}
      {error.detail ? <span className="mono"> {error.detail}</span> : null}
    </p>
  );
}

export function OwnershipBar({ session, active }: { session: Session; active: boolean }) {
  const { t } = useI18n();
  const owner = ownerWord(session, t);
  const cont = useContinue(session);
  const anchor = useRef<HTMLButtonElement>(null);
  const whyId = useId();
  const [why, setWhy] = useState(false);
  const [focusError, setFocusError] = useState<ErrLine>(null);
  if (session.control.kind !== 'watching') return null;
  const origin = session.control.origin;

  let sentence: string;
  if (origin === 'background') sentence = t('observed.background');
  else if (origin === 'headless') sentence = t('observed.headless');
  else sentence = t('observed.banner', { owner });
  const waiting = session.pending.find((p) => p.kind === 'watchedApproval' && (p.tool?.verb === 'edit' || p.tool?.verb === 'create'));

  const canAttach = session.caps.continueOptions.includes('attach');
  const canFork = session.caps.continueOptions.includes('fork');
  const primary: { label: string; run: () => void } | null = canAttach
    ? { label: t('action.openInGilde'), run: () => cont.run({ kind: 'attach' }) }
    : session.caps.ownerFocus
      ? {
          label: t('action.goToOwner', { owner }),
          run: () => void commands.owner_focus({ uid: session.uid }).then(() => setFocusError(null), setFocusError),
        }
      : null;

  return (
    <BarShortcut active={active && primary !== null && session.pending.length === 0} run={() => primary?.run()}>
      <div className="g-bar" role="region" aria-label={t('session.ownerBar')} data-bar="ownership">
        <p className="g-bar-text">
          {sentence}
          {waiting?.tool?.object ? ` ${t('observed.waitingEdit', { file: waiting.tool.object })}` : ''}
        </p>
        <div className="g-bar-actions">
          {primary && (
            <button type="button" className="g-sbtn g-sbtn-primary" aria-keyshortcuts="Meta+Enter" disabled={cont.busy} onClick={primary.run}>
              {primary.label} <span className="g-key" aria-hidden="true">⌘⏎</span>
            </button>
          )}
          {canFork && (
            <button type="button" className="g-sbtn" disabled={cont.busy} onClick={() => cont.run({ kind: 'fork' })}>
              {t('action.continueCopy')}
            </button>
          )}
          <span className="g-bar-spacer" />
          <button ref={anchor} type="button" className="g-linkbtn" {...popoverTriggerProps(why, whyId)} onClick={() => setWhy((w) => !w)}>
            {t('observed.whyLink')}
          </button>
          <Popover open={why} anchor={anchor} onClose={() => setWhy(false)} id={whyId} label={t('observed.whyLink')}>
            <p className="g-bar-why">{t('observed.why', { owner })}</p>
          </Popover>
        </div>
        <ErrorText error={cont.error ?? focusError} />
      </div>
    </BarShortcut>
  );
}

export function ContinueBar({ session, active }: { session: Session; active: boolean }) {
  const { t, locale } = useI18n();
  const cont = useContinue(session);
  const [ideError, setIdeError] = useState<ErrLine>(null);
  if (session.control.kind !== 'notRunning') return null;
  const resumable = session.caps.continueOptions.includes('resume');
  const forkOnly = !resumable && session.caps.continueOptions.includes('fork');
  const at = session.endedAt ?? session.lastActivityAt;
  const openFolder = () => commands.ide_open({ target: { kind: 'folder', uid: session.uid } }).then(() => setIdeError(null), setIdeError);
  const primary = resumable
    ? () => cont.run({ kind: 'resume' })
    : forkOnly
      ? () => cont.run({ kind: 'fork' })
      : () => void openFolder();
  return (
    <BarShortcut active={active} run={primary}>
      <div className="g-bar" role="region" aria-label={t('session.ownerBar')} data-bar="continue">
        <p className="g-bar-text">
          {resumable || forkOnly ? t('ended.banner', { time: at ? formatTimeOfDay(at, locale) : t('common.unknown') }) : t('ended.notResumable')}
        </p>
        <div className="g-bar-actions">
          {resumable || forkOnly ? (
            <button type="button" className="g-sbtn g-sbtn-primary" aria-keyshortcuts="Meta+Enter" disabled={cont.busy} onClick={primary}>
              {resumable ? t('action.continue') : t('action.continueCopy')} <span className="g-key" aria-hidden="true">⌘⏎</span>
            </button>
          ) : null}
          <button type="button" className={`g-sbtn${resumable || forkOnly ? '' : ' g-sbtn-primary'}`} onClick={() => void openFolder()}>
            {t('action.openInIde')}
          </button>
        </div>
        <ErrorText error={cont.error ?? ideError} />
      </div>
    </BarShortcut>
  );
}

/** The In Gilde error state's primary action (ux-ia §4.3). Null when the state has none here. */
export function ErrorBar({ session, onShowLog }: { session: Session; onShowLog: () => void }) {
  const { t } = useI18n();
  const accounts = useAppStore((s) => s.world.accounts);
  const openSession = useAppStore((s) => s.openSession);
  const cont = useContinue(session);
  const [error, setError] = useState<ErrLine>(null);
  const [busy, setBusy] = useState(false);
  const state = session.activity.state;
  if (state.kind !== 'error' || session.control.kind !== 'inGilde') return null;

  const call = (p: Promise<unknown>) => {
    setBusy(true);
    p.then(
      () => {
        setBusy(false);
        setError(null);
      },
      (e: IpcError) => {
        setBusy(false);
        setError(e);
      },
    );
  };

  let action: { label: string; run: () => void } | null = null;
  let hint: string | null = null;
  switch (state.sub.kind) {
    case 'usageLimit': {
      const other = [...accounts.values()].find((a) => a.vendor === session.vendor && a.id !== session.accountId && a.usableForNew);
      if (other && session.caps.continueOptions.includes('otherAccount')) {
        action = { label: t('action.continueOn', { account: other.label }), run: () => cont.run({ kind: 'otherAccount', accountId: other.id }) };
      }
      break;
    }
    case 'signInNeeded':
      if (session.accountId) {
        const accountId = session.accountId;
        action = {
          label: t('action.signIn'),
          run: () =>
            commands.account_sign_in({ clientRequestId: newClientRequestId(), accountId }).then(
              (res) => openSession(res.uid, { sub: 'terminal' }),
              (e: IpcError) => setError(e),
            ),
        };
      }
      break;
    case 'didntStart':
      if ((state.sub.reason === 'compatFailed' || state.sub.reason === 'minVersion') && session.caps.continueOptions.includes('retryAsTerminal')) {
        hint = t('session.retryAsTerminalHint');
        action = {
          label: t('action.startAsTerminal'),
          run: () => call(commands.session_retry_as_terminal({ clientRequestId: newClientRequestId(), uid: session.uid })),
        };
      } else if (session.caps.terminalView || session.caps.logView) {
        action = { label: t('action.showLog'), run: onShowLog };
      }
      break;
    case 'agentError':
      action = { label: t('action.retry'), run: () => call(commands.turn_retry({ uid: session.uid, clientMsgId: newClientMsgId() })) };
      break;
  }
  if (!action && !hint) return null;
  return (
    <div className="g-bar g-bar-error" data-bar="error">
      {hint && <p className="g-bar-text">{hint}</p>}
      {action && (
        <div className="g-bar-actions">
          <button type="button" className="g-sbtn g-sbtn-primary" disabled={busy || cont.busy} onClick={action.run}>
            {action.label}
          </button>
        </div>
      )}
      <ErrorText error={error ?? cont.error} />
    </div>
  );
}

/** `⌘⏎` runs a bar's primary action while the session view is active and no card claims it. */
function BarShortcut({ active, run, children }: { active: boolean; run: () => void; children: ReactNode }) {
  useHotkeys([{ id: 'answer.primary', combo: 'mod+enter', run }], { scope: 'S', enabled: active });
  return <>{children}</>;
}
