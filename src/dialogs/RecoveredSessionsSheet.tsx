/**
 * Recovered Sessions sheet (T26; DESIGN §8-5, §3 `monitor / recovery`).
 *
 * The boot sweep runs before the window exists. What it could not decide on its
 * own — a live process from a crashed instance, still holding a worktree and a
 * gigabyte of RSS — lands here, in front of the main UI, *before* the user
 * starts a new session on top of it.
 *
 * Three rules shape the whole component:
 *
 *  1. **It does not close without a decision.** There is no close button, Escape does
 *     nothing and the backdrop is inert; "모두 유지" is an explicit button, so
 *     keeping everything is a choice the user makes rather than one they fall
 *     into. `Terminate` is the pre-selected action per row (DESIGN §8-5), which
 *     is why one click on 적용 resolves the sheet.
 *  2. **Every row states the facts before it offers a verb**: vendor, the argv
 *     as Rust masked it (secrets never reach this process unredacted), the cwd,
 *     the worktree, RSS and age.
 *  3. **The result is reported honestly, per row.** `recovery_resolve` answers a
 *     `KillOutcome` for every decision — pids signalled, pids excluded and why,
 *     survivors, escalations, and the identity fence that authorised the kill.
 *     A row that was spared says so; a survivor is never rendered as a success.
 */

import { useCallback, useEffect, useMemo, useState } from 'react';

import { Badge } from '../components/Badge';
import { Mono } from '../components/Mono';
import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey, MessageParams } from '../i18n';
import { cmd, isUnavailable, asGildeError } from '../lib/commands';
import { formatBytes, formatDuration, formatNumber, truncatePath } from '../lib/format';
import type {
  GildeErrorPayload,
  KillOutcome,
  Locale,
  RecoveredProc,
  RecoveryAction,
  RecoveryDecision,
} from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import './dialogs.css';

/** Offered per row, in the order the sheet presents them. */
export const RECOVERY_ACTIONS: readonly RecoveryAction[] = ['terminate', 'keep', 'adopt'];

const ACTION_LABEL: Record<RecoveryAction, MessageKey> = {
  terminate: 'recovery.action.terminate',
  keep: 'recovery.action.keep',
  adopt: 'recovery.action.adopt',
};

const ACTION_WHAT: Record<RecoveryAction, MessageKey> = {
  terminate: 'recovery.action.terminate.what',
  keep: 'recovery.action.keep.what',
  adopt: 'recovery.action.adopt.what',
};

export type ChoiceMap = Record<string, RecoveryAction>;

/** The pre-selected action is whatever Rust suggested — always `terminate`. */
export function initialChoices(rows: readonly RecoveredProc[]): ChoiceMap {
  const choices: ChoiceMap = {};
  for (const row of rows) choices[row.spawnId] = row.suggested;
  return choices;
}

export function choiceOf(row: RecoveredProc, choices: ChoiceMap): RecoveryAction {
  return choices[row.spawnId] ?? row.suggested;
}

/** One decision per row, in the order shown — the wire shape of `RecoveryDecision`. */
export function decisionsFrom(
  rows: readonly RecoveredProc[],
  choices: ChoiceMap,
): RecoveryDecision[] {
  return rows.map((row) => ({ spawnId: row.spawnId, action: choiceOf(row, choices) }));
}

/** "모두 유지" is one button, not a per-row sweep the user has to repeat. */
export function keepAllDecisions(rows: readonly RecoveredProc[]): RecoveryDecision[] {
  return rows.map((row) => ({ spawnId: row.spawnId, action: 'keep' as RecoveryAction }));
}

// ---------------------------------------------------------------------------
// Outcome rendering — the honest part
// ---------------------------------------------------------------------------

export interface OutcomeSummary {
  tone: 'ok' | 'warn' | 'error' | undefined;
  /** One human sentence, already interpolated. */
  text: string;
  /** Raw backend tokens (fence, refusal reason, exclusion reasons), verbatim. */
  facts: string[];
}

/**
 * Turn a `KillOutcome` into what the row says.
 *
 * The decision the sheet sent is passed in rather than parsed back out of the
 * report: `Keep` and `Adopt` come back as a *refused* kill (there was nothing to
 * kill), and rendering that as a refusal would be a lie.
 */
export function outcomeSummary(
  outcome: KillOutcome | undefined,
  action: RecoveryAction,
  t: (key: MessageKey, params?: MessageParams) => string,
  locale: Locale,
): OutcomeSummary | null {
  if (!outcome) return null;
  const report = outcome.report;
  const facts: string[] = [];
  if (report.fence) facts.push(report.fence);
  if (report.refusedReason) facts.push(report.refusedReason);
  for (const excluded of report.excluded) facts.push(`${excluded.pid} ${excluded.reason}`);

  if (action !== 'terminate') {
    const notFound = report.refusedReason === 'notFound';
    return {
      tone: notFound ? 'warn' : undefined,
      text: t(
        notFound
          ? 'recovery.outcome.gone'
          : action === 'keep'
            ? 'recovery.outcome.kept'
            : 'recovery.outcome.adopted',
      ),
      facts,
    };
  }

  if (report.refusedReason) {
    return { tone: 'warn', text: t('recovery.outcome.refused'), facts };
  }

  const survivors = report.survivors.length;
  const text = t('recovery.outcome.terminated', {
    requested: formatNumber(report.requested.length, locale),
    excluded: formatNumber(report.excluded.length, locale),
    escalated: formatNumber(report.escalated.length, locale),
    survivors: formatNumber(survivors, locale),
  });
  return { tone: survivors > 0 ? 'error' : 'ok', text, facts };
}

/**
 * The rows still waiting for a decision.
 *
 * `recovery_pending` is recomputed against liveness on every call, so a row the
 * user has already answered can come back for one more poll before the backend
 * drops it; `acknowledged` is what keeps the sheet from reopening on top of the
 * app the moment it was closed.
 */
export function outstandingRows(
  rows: readonly RecoveredProc[],
  acknowledged: readonly string[],
): RecoveredProc[] {
  return rows.filter((row) => !acknowledged.includes(row.spawnId));
}

/** Rows are keyed by `spawnId`; the backend answers with a `spawn` target. */
export function outcomeFor(
  outcomes: readonly KillOutcome[] | null,
  spawnId: string,
): KillOutcome | undefined {
  if (!outcomes) return undefined;
  return outcomes.find((o) => o.target.target === 'spawn' && o.target.spawnId === spawnId);
}

// ---------------------------------------------------------------------------
// The card (rendered without a portal, so the static tests can mount it)
// ---------------------------------------------------------------------------

export interface RecoveredSessionsCardProps {
  rows: readonly RecoveredProc[];
  choices: ChoiceMap;
  outcomes: readonly KillOutcome[] | null;
  busy: boolean;
  error: GildeErrorPayload | null;
  onChoose: (spawnId: string, action: RecoveryAction) => void;
}

export function RecoveredSessionsCard({
  rows,
  choices,
  outcomes,
  busy,
  error,
  onChoose,
}: RecoveredSessionsCardProps) {
  const { t, td, locale } = useI18n();

  return (
    <div className="rec">
      <span className="rec-intro">
        {t(outcomes ? 'recovery.resolved' : 'recovery.intro', {
          n: formatNumber(rows.length, locale),
        })}
      </span>

      <div className="rec-list">
        {rows.map((row) => {
          const action = choiceOf(row, choices);
          const summary = outcomeSummary(outcomeFor(outcomes, row.spawnId), action, t, locale);
          return (
            <div key={row.spawnId} className="rec-row">
              <Badge title={t(`recovery.kind.${row.kind}` as MessageKey)}>
                {row.vendor ?? row.kind}
              </Badge>

              <div className="rec-main">
                <Mono small ellipsis title={row.argvDisplay}>
                  {row.argvDisplay}
                </Mono>
                <div className="rec-meta">
                  <Mono small ellipsis title={row.cwd}>
                    {truncatePath(row.cwd)}
                  </Mono>
                  {row.worktreeId ? <Badge title={row.worktreeId}>{t('node.worktree')}</Badge> : null}
                  {row.keepAlive ? <Badge tone="warn">{t('badge.detached')}</Badge> : null}
                </div>
              </div>

              <div className="rec-metrics">
                <Mono small>{formatBytes(row.rssBytes, locale)}</Mono>
                <Mono small>{formatDuration(row.ageSecs * 1000, locale)}</Mono>
              </div>

              {outcomes ? (
                <span className="s11">{t(ACTION_LABEL[action])}</span>
              ) : (
                <div className="seg" role="group" aria-label={t('recovery.action')}>
                  {RECOVERY_ACTIONS.map((candidate) => (
                    <button
                      key={candidate}
                      type="button"
                      aria-pressed={action === candidate}
                      disabled={busy}
                      title={t(ACTION_WHAT[candidate])}
                      onClick={() => onChoose(row.spawnId, candidate)}
                    >
                      {t(ACTION_LABEL[candidate])}
                    </button>
                  ))}
                </div>
              )}

              {summary ? (
                <div className="rec-outcome" data-tone={summary.tone}>
                  <span>{summary.text}</span>
                  {summary.facts.length > 0 ? (
                    <Mono small className="muted">
                      {summary.facts.join(' ')}
                    </Mono>
                  ) : null}
                </div>
              ) : null}
            </div>
          );
        })}
      </div>

      {error ? <span className="set-issue">{td(error.key, error.params)}</span> : null}
      {error?.detail ? <pre className="mono s11 muted empty-detail">{error.detail}</pre> : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// The sheet
// ---------------------------------------------------------------------------

export function RecoveredSessionsSheet() {
  const { t } = useI18n();
  const pendingCount = useAppStore((s) => s.recoveryPending);

  const [rows, setRows] = useState<RecoveredProc[]>([]);
  const [submitted, setSubmitted] = useState<RecoveredProc[]>([]);
  const [choices, setChoices] = useState<ChoiceMap>({});
  const [outcomes, setOutcomes] = useState<KillOutcome[] | null>(null);
  const [acknowledged, setAcknowledged] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<GildeErrorPayload | null>(null);

  const load = useCallback(async () => {
    try {
      const pending = await cmd.recoveryPending();
      setRows(pending);
      setChoices((prev) => ({ ...initialChoices(pending), ...prev }));
    } catch (e) {
      // The command lands with T11; until then there is simply nothing to show.
      if (!isUnavailable(e)) console.warn('[gilde] recovery_pending failed', e);
    }
  }, []);

  // The sweep publishes `gilde://recovery/pending`, which moves the store counter;
  // the rows themselves are always fetched, because `app_bootstrap` reports the
  // count and not the list.
  useEffect(() => {
    void load();
  }, [load, pendingCount]);

  const outstanding = useMemo(
    () => outstandingRows(rows, acknowledged),
    [rows, acknowledged],
  );

  const shown = outcomes ? submitted : outstanding;
  const open = outcomes !== null || outstanding.length > 0;

  const resolve = useCallback(
    async (decisions: RecoveryDecision[], snapshot: RecoveredProc[]) => {
      setBusy(true);
      setError(null);
      try {
        const result = await cmd.recoveryResolve(decisions);
        setSubmitted(snapshot);
        setChoices((prev) => {
          const next = { ...prev };
          for (const decision of decisions) next[decision.spawnId] = decision.action;
          return next;
        });
        setOutcomes(result);
        await load();
      } catch (e) {
        setError(asGildeError(e));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  const acknowledge = useCallback(() => {
    setAcknowledged((prev) => [...prev, ...submitted.map((row) => row.spawnId)]);
    setOutcomes(null);
    setSubmitted([]);
  }, [submitted]);

  if (!open) return null;

  return (
    <Sheet
      open={open}
      title={t('recovery.title')}
      // No decision, no exit: Escape and the backdrop are inert until the sheet
      // has an answer for every row (DESIGN §8-5).
      onClose={outcomes ? acknowledge : () => undefined}
      actions={
        outcomes ? (
          <button type="button" className="btn primary" onClick={acknowledge}>
            {t('common.close')}
          </button>
        ) : (
          <>
            {/* Focused first (Sheet focuses the first action): the safe answer. */}
            <button
              type="button"
              className="btn"
              disabled={busy}
              onClick={() => void resolve(keepAllDecisions(outstanding), outstanding)}
            >
              {t('recovery.keepAll')}
            </button>
            <button
              type="button"
              className="btn danger"
              disabled={busy}
              onClick={() => void resolve(decisionsFrom(outstanding, choices), outstanding)}
            >
              {busy ? t('recovery.applying') : t('recovery.apply')}
            </button>
          </>
        )
      }
    >
      <RecoveredSessionsCard
        rows={shown}
        choices={choices}
        outcomes={outcomes}
        busy={busy}
        error={error}
        onChoose={(spawnId, action) => setChoices((prev) => ({ ...prev, [spawnId]: action }))}
      />
    </Sheet>
  );
}
