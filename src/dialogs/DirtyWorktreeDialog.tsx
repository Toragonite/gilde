/**
 * Worktree removal (M1-PLAN T21 acceptance 6, DESIGN §7-7).
 *
 * This is the data-loss gate, so it is a card of facts before it is a set of
 * buttons: what is uncommitted, how much changed, how many commits are not
 * merged, which sessions are still attached, which alias links exist and which
 * foreign processes have their cwd inside. All of it comes from
 * `worktree_remove_preflight`, gathered before anything is destroyed.
 *
 * Exactly three modes exist — `KeepBranch`, `CommitThenRemove`,
 * `ArchiveThenRemove` — and `Force` deliberately is not one of them. A mode the
 * backend did not put in `allowedModes` is rendered disabled with the reason,
 * not hidden: "why can't I" is the question the user actually has.
 *
 * **No destructive action in this app is a bare `confirm()`.** The default
 * focus is Cancel (`Sheet` focuses the first action), and the branch is never
 * deleted, which the card says out loud.
 *
 * `RemoveWorktreeCard` is exported separately from the sheet because the sheet
 * renders through a DOM portal, which the static-render tests cannot mount.
 */

import { useState } from 'react';

import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatNumber } from '../lib/format';
import type { Locale, RemoveMode, RemovePreflight } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import type { RemoveDialogState } from '../store/worktreeSlice';
import '../sidebar/sidebar.css';

/** DESIGN §7-7, in the order the card offers them. */
export const REMOVE_MODES: readonly RemoveMode[] = [
  'keepBranch',
  'commitThenRemove',
  'archiveThenRemove',
];

const MODE_NAME: Record<RemoveMode, MessageKey> = {
  keepBranch: 'worktree.mode.keepBranch',
  commitThenRemove: 'worktree.mode.commitThenRemove',
  archiveThenRemove: 'worktree.mode.archiveThenRemove',
};

const MODE_WHAT: Record<RemoveMode, MessageKey> = {
  keepBranch: 'worktree.mode.keepBranch.what',
  commitThenRemove: 'worktree.mode.commitThenRemove.what',
  archiveThenRemove: 'worktree.mode.archiveThenRemove.what',
};

/**
 * Why a mode the backend refused is refused. The preflight facts decide, in the
 * same order §7-7 guards them: live sessions block every mode, a dirty tree
 * blocks `KeepBranch`.
 */
export function disabledReason(
  mode: RemoveMode,
  preflight: RemovePreflight | null,
): MessageKey | null {
  if (!preflight) return 'common.loading';
  if (preflight.allowedModes.includes(mode)) return null;
  if (preflight.liveSessions.length > 0) return 'worktree.blocked.sessions';
  if (preflight.runningRuns.length > 0) return 'worktree.blocked.runs';
  if (preflight.foreignPids.length > 0) return 'worktree.blocked.foreign';
  if (mode === 'keepBranch' && (preflight.dirtyFiles > 0 || preflight.untracked > 0)) {
    return 'worktree.blocked.dirty';
  }
  return 'worktree.blocked.other';
}

interface Fact {
  key: MessageKey;
  value: string;
  tone?: 'warn' | 'error';
}

/** Everything the card states, derived only from the preflight and the stats. */
export function removalFacts(
  state: RemoveDialogState,
  t: (key: MessageKey, params?: Record<string, string | number>) => string,
  locale: Locale,
): Fact[] {
  const preflight = state.preflight;
  if (!preflight) return [];
  const num = (n: number) => formatNumber(n, locale);
  return [
    {
      key: 'worktree.fact.dirty',
      value: t('worktree.fact.dirty.value', {
        dirty: num(preflight.dirtyFiles),
        untracked: num(preflight.untracked),
      }),
      tone: preflight.dirtyFiles + preflight.untracked > 0 ? 'warn' : undefined,
    },
    {
      key: 'worktree.fact.diff',
      value: state.stats
        ? `+${num(state.stats.added)} −${num(state.stats.removed)}`
        : t('common.unknown'),
    },
    {
      key: 'worktree.fact.unmerged',
      value: num(preflight.unmergedCommits),
      tone: preflight.unmergedCommits > 0 ? 'warn' : undefined,
    },
    {
      key: 'worktree.fact.sessions',
      value:
        preflight.liveSessions.length === 0 ? t('common.none') : preflight.liveSessions.join(' '),
      tone: preflight.liveSessions.length > 0 ? 'error' : undefined,
    },
    {
      key: 'worktree.fact.runs',
      value:
        preflight.runningRuns.length === 0 ? t('common.none') : preflight.runningRuns.join(' '),
      tone: preflight.runningRuns.length > 0 ? 'error' : undefined,
    },
    {
      key: 'worktree.fact.alias',
      value: preflight.aliasLinks.length === 0 ? t('common.none') : preflight.aliasLinks.join('\n'),
    },
    {
      key: 'worktree.fact.foreign',
      value:
        preflight.foreignPids.length === 0
          ? t('common.none')
          : preflight.foreignPids.map((pid) => String(pid)).join(' '),
      tone: preflight.foreignPids.length > 0 ? 'warn' : undefined,
    },
  ];
}

export interface RemoveWorktreeCardProps {
  state: RemoveDialogState;
  selected: RemoveMode | null;
  onPick: (mode: RemoveMode) => void;
}

export function RemoveWorktreeCard({ state, selected, onPick }: RemoveWorktreeCardProps) {
  const { t, td, locale } = useI18n();
  const preflight = state.preflight;
  const facts = removalFacts(state, t, locale);

  return (
    <div className="sb-form">
      <div className="sb-card">
        <div className="row gap-8">
          <span className="mono s12">{state.label}</span>
          <span className="muted mono s11">{state.branch}</span>
        </div>

        {state.loading ? (
          <span className="muted s12">{t('common.loading')}</span>
        ) : preflight ? (
          <div className="sb-facts">
            {facts.map((fact) => (
              <div key={fact.key} className="contents">
                <span className="sb-fact-k">{t(fact.key)}</span>
                <span className="sb-fact-v" data-tone={fact.tone}>
                  {fact.value}
                </span>
              </div>
            ))}
          </div>
        ) : (
          <span className="sb-issue">{t('worktree.preflightUnavailable')}</span>
        )}

        <span className="sb-hint">{t('worktree.branchKept', { branch: state.branch })}</span>
      </div>

      <div className="sb-modes" role="group" aria-label={t('worktree.remove')}>
        {REMOVE_MODES.map((candidate) => {
          const reason = disabledReason(candidate, preflight);
          return (
            <button
              key={candidate}
              type="button"
              className="sb-mode"
              aria-pressed={selected === candidate}
              disabled={reason !== null || state.busy}
              title={reason ? t(reason) : undefined}
              onClick={() => onPick(candidate)}
            >
              <span className="sb-mode-body">
                <span className="sb-mode-name">{t(MODE_NAME[candidate])}</span>
                <span className="sb-mode-why">{reason ? t(reason) : t(MODE_WHAT[candidate])}</span>
              </span>
            </button>
          );
        })}
      </div>

      {state.error ? (
        <span className="sb-issue">{td(state.error.key, state.error.params)}</span>
      ) : null}
      {state.error?.detail ? (
        <pre className="mono s11 muted empty-detail">{state.error.detail}</pre>
      ) : null}
    </div>
  );
}

export function DirtyWorktreeDialog() {
  const { t } = useI18n();

  const state = useAppStore((s) => s.worktree.remove);
  const close = useAppStore((s) => s.closeRemoveDialog);
  const confirmRemove = useAppStore((s) => s.confirmRemove);

  const [mode, setMode] = useState<RemoveMode | null>(null);

  if (!state.open) return null;

  const allowed = state.preflight?.allowedModes ?? [];
  const chosen = mode && allowed.includes(mode) ? mode : null;

  return (
    <Sheet
      open={state.open}
      title={t('worktree.remove')}
      onClose={close}
      actions={
        <>
          {/* Cancel first: `Sheet` focuses it (UI-GUIDELINES §2-5). */}
          <button type="button" className="btn" onClick={close}>
            {t('common.cancel')}
          </button>
          <button
            type="button"
            className="btn danger"
            disabled={!chosen || state.busy}
            onClick={() => {
              if (chosen) void confirmRemove(chosen);
            }}
          >
            {state.busy ? t('worktree.removing') : t('worktree.remove.submit')}
          </button>
        </>
      }
    >
      <RemoveWorktreeCard state={state} selected={chosen} onPick={setMode} />
    </Sheet>
  );
}
