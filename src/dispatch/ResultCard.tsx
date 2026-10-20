/**
 * One run, one card (DESIGN §9-7).
 *
 *   header  vendor · model_actual · account · worktree/branch · status · elapsed
 *   body    streamed text (last ~40 lines, auto-scroll, expandable) + tool ticker
 *   footer  in/out/cached · $cost · conversationId · route note · adapter note
 *   actions Stop · Resume · Open terminal here · Open worktree · Copy
 *
 * Two claims this file is careful about:
 *
 *  1. **`interrupted` is not an error.** The sidecar generation that owned the
 *     run died under it (DESIGN §5-10): the agent did nothing wrong, and the
 *     card says the app restarted and offers "continue the conversation" or
 *     "run it again" instead of a red failure (acceptance 5).
 *  2. **The model shown is the one that actually ran.** `modelActual` can
 *     differ from what was asked for; showing the request would be a lie the
 *     user would only discover in the bill.
 */

import { useEffect, useState } from 'react';

import { Badge, Chip } from '../components/Badge';
import { Mono } from '../components/Mono';
import { StatusPill } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatElapsedClock } from '../lib/format';
import type { GraphNode, NodeStatus, RunStatus } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import {
  copyToClipboard,
  elapsedMs,
  isResumable,
  isTerminalRun,
} from '../store/dispatchSlice';
import type { RunView } from '../store/dispatchSlice';
import { RunTimeline } from './RunTimeline';
import { UsageChips } from './UsageChips';
import { agentDisplayName } from '../lib/agents';

// ---------------------------------------------------------------------------
// Status presentation
// ---------------------------------------------------------------------------

/**
 * Run status → the shared status token (UI-GUIDELINES §2-1a). There is no
 * colour of its own for a dispatch run: `interrupted` and `incomplete` are
 * amber "needs you", `error` is the only red, and everything finished-but-not
 * -successful is neutral. The label always travels with the colour.
 */
export function runNodeStatus(status: RunStatus): NodeStatus {
  switch (status) {
    case 'queued':
    case 'spawning':
      return 'idle';
    case 'running':
      return 'working';
    case 'done':
      return 'done';
    case 'error':
      return 'error';
    case 'interrupted':
    case 'incomplete':
      return 'stalled';
    case 'stopped':
    case 'cancelled':
      return 'exited';
  }
}

export function runStatusKey(status: RunStatus): MessageKey {
  return `dispatch.run.${status}` as MessageKey;
}

// ---------------------------------------------------------------------------
// Shared 1 s ticker — one timer for every elapsed clock on screen
// ---------------------------------------------------------------------------

const tickListeners = new Set<() => void>();
let ticker: ReturnType<typeof setInterval> | null = null;

export function useElapsedTick(enabled: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!enabled) return;
    const listener = () => setNow(Date.now());
    tickListeners.add(listener);
    if (!ticker) {
      ticker = setInterval(() => {
        for (const fn of tickListeners) fn();
      }, 1000);
    }
    return () => {
      tickListeners.delete(listener);
      if (tickListeners.size === 0 && ticker) {
        clearInterval(ticker);
        ticker = null;
      }
    };
  }, [enabled]);
  return now;
}

// ---------------------------------------------------------------------------
// Card
// ---------------------------------------------------------------------------

export interface ResultCardProps {
  view: RunView;
  focused: boolean;
  expanded: boolean;
  compare: boolean;
  worktree?: GraphNode;
  onFocus: () => void;
  registerBody?: (element: HTMLPreElement | null) => void;
  onBodyScroll?: (element: HTMLPreElement) => void;
}

export function ResultCard({
  view,
  focused,
  expanded,
  compare,
  worktree,
  onFocus,
  registerBody,
  onBodyScroll,
}: ResultCardProps) {
  const { t } = useI18n();
  const run = view.run;
  const finished = isTerminalRun(run.status);
  const now = useElapsedTick(!finished);

  const cancelRun = useAppStore((s) => s.cancelRun);
  const resumeRun = useAppStore((s) => s.resumeRun);
  const rerunRun = useAppStore((s) => s.rerunRun);
  const openRunTerminal = useAppStore((s) => s.openRunTerminal);
  const revealRunWorktree = useAppStore((s) => s.revealRunWorktree);
  const copyRunResult = useAppStore((s) => s.copyRunResult);
  const toggleRunExpanded = useAppStore((s) => s.toggleRunExpanded);
  const pushToast = useAppStore((s) => s.pushToast);

  const copyConversationId = (conversationId: string) => {
    void copyToClipboard(conversationId).then((ok) =>
      pushToast({
        level: ok ? 'success' : 'warn',
        key: ok ? 'dispatch.card.copiedId' : 'dispatch.card.copyFailed',
        params: {},
        detail: null,
        actionId: null,
        undoToken: null,
        ttlMs: 4000,
      }),
    );
  };

  const [resumeOpen, setResumeOpen] = useState(false);
  const [resumeText, setResumeText] = useState('');

  const status = runNodeStatus(run.status);
  const elapsed = formatElapsedClock(elapsedMs(run, now));
  const resumable = isResumable(run);
  const interrupted = run.status === 'interrupted';

  const submitResume = () => {
    const message = resumeText.trim();
    if (!message) return;
    setResumeOpen(false);
    setResumeText('');
    void resumeRun(run.id, message);
  };

  return (
    <section
      className="dsp-card"
      data-focused={focused ? 'true' : undefined}
      data-status={run.status}
      aria-label={`${agentDisplayName(run.vendor)} · ${t(runStatusKey(run.status))}`}
      onMouseDown={onFocus}
    >
      <header className="row gap-8 dsp-card-hdr">
        <Badge>{agentDisplayName(run.vendor)}</Badge>
        <Mono small className="muted ell" title={run.modelRequested ?? undefined}>
          {run.modelActual ?? t('dispatch.card.modelUnknown')}
        </Mono>
        <Chip title={t('dispatch.card.account')}>
          {run.accountId ?? t('dispatch.card.ambient')}
        </Chip>
        {worktree ? (
          <Chip title={worktree.meta.branch ?? undefined}>
            <Mono small ellipsis>
              {worktree.meta.branch ?? worktree.label}
            </Mono>
          </Chip>
        ) : null}
        <div className="grow" />
        <StatusPill status={status} label={t(runStatusKey(run.status))} />
        <Mono small className="muted">
          {elapsed}
        </Mono>
      </header>

      {interrupted ? (
        <p className="s12 dsp-interrupted">{t('dispatch.card.interrupted')}</p>
      ) : null}

      {run.status === 'error' && run.error ? (
        <p className="s12 dsp-error">
          <span className="muted">{t('dispatch.card.error')}</span> {run.error}
        </p>
      ) : null}

      <RunTimeline
        text={view.stream}
        tool={view.tool}
        expanded={expanded}
        finished={finished}
        compare={compare}
        registerBody={registerBody}
        onBodyScroll={onBodyScroll}
      />

      {run.textPath ? (
        <Mono small className="muted ell" title={run.textPath}>
          {t('dispatch.card.spill', { path: run.textPath })}
        </Mono>
      ) : null}

      <UsageChips
        run={run}
        usage={view.usage}
        costUsd={view.costUsd}
        onCopyConversationId={copyConversationId}
      />

      {resumeOpen ? (
        <div className="col gap-6">
          <textarea
            className="textarea dsp-resume"
            value={resumeText}
            // Nothing visible names this field — the disclosure button above
            // it is the only text — so the name goes on the control itself
            // (BACKLOG 11, ui/a11y-input-label).
            aria-label={t('dispatch.card.resumePlaceholder')}
            placeholder={t('dispatch.card.resumePlaceholder')}
            onChange={(event) => setResumeText(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && event.metaKey) {
                event.preventDefault();
                submitResume();
              }
            }}
          />
          <div className="row gap-6">
            <div className="grow" />
            <button type="button" className="btn" onClick={() => setResumeOpen(false)}>
              {t('common.cancel')}
            </button>
            <button
              type="button"
              className="btn primary"
              disabled={resumeText.trim().length === 0}
              onClick={submitResume}
            >
              {t('dispatch.card.resumeSend')}
            </button>
          </div>
        </div>
      ) : null}

      <div className="row gap-6 dsp-actions">
        <button type="button" className="btn ghost" onClick={() => toggleRunExpanded(run.id)}>
          {expanded ? t('dispatch.card.collapse') : t('dispatch.card.expand')}
        </button>
        <div className="grow" />

        {!finished ? (
          <button type="button" className="btn danger" onClick={() => void cancelRun(run.id)}>
            {t('dispatch.card.stop')}
          </button>
        ) : null}

        {/* Resume needs a conversation id (acceptance 6). The one exception is
            an interrupted run, where acceptance 5 requires the choice to be
            offered — there it is shown disabled, with the reason. */}
        {finished && (resumable || interrupted) ? (
          <button
            type="button"
            className="btn"
            disabled={!resumable}
            title={resumable ? undefined : t('dispatch.card.resumeDisabled')}
            onClick={() => setResumeOpen(true)}
          >
            {t('dispatch.card.resume')}
          </button>
        ) : null}

        {interrupted ? (
          <button type="button" className="btn" onClick={() => void rerunRun(run.id)}>
            {t('dispatch.card.rerun')}
          </button>
        ) : null}

        <button type="button" className="btn" onClick={() => void openRunTerminal(run.id)}>
          {t('dispatch.card.openTerminal')}
        </button>
        <button type="button" className="btn" onClick={() => void revealRunWorktree(run.id)}>
          {t('dispatch.card.openWorktree')}
        </button>
        <button type="button" className="btn" onClick={() => void copyRunResult(run.id)}>
          {t('dispatch.card.copy')}
        </button>
      </div>
    </section>
  );
}
