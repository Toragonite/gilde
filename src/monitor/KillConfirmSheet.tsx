/**
 * Kill confirmation sheet (T23, DESIGN §8-6, §8-7).
 *
 * The sheet exists to make one question answerable before the signal: *what
 * exactly is about to happen*. So it lists
 *
 *   - the exact pids that will be signalled, with their masked argv;
 *   - the escalation, with the user's own grace window: SIGTERM to the process
 *     group → wait `killGraceMs` → SIGKILL → verify for 2 s → survivors;
 *   - a **dry-run toggle** that renders the target and exclusion sets Rust
 *     itself computed, without sending anything.
 *
 * And afterwards it reports the `KillOutcome`s honestly — "6개 종료, 1개
 * 거부(argv 불일치), 1개 SIGKILL 생존" — because a refusal, a failure and a
 * survivor are three different facts.
 *
 * Default focus is Cancel (UI-GUIDELINES §2-5). No destructive path in this app
 * goes through a bare `confirm()`.
 */

import { Sheet } from '../components/Sheet';
import { Mono } from '../components/Mono';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatNumber } from '../lib/format';
import type { KillOutcome } from '../lib/ipc';
import { dryRunSets, tallyOutcomes } from '../store/procSlice';
import type { DryRunSets, KillPlanState } from '../store/procSlice';
import { useAppStore } from '../store/useAppStore';
// The sheet is mounted globally (AppShell), so it brings its own styles.
import './monitor.css';

/** Rust's grace default when settings have not been read yet (DESIGN §8-6). */
const FALLBACK_GRACE_MS = 5_000;

export function killReasonKey(reason: string): MessageKey {
  return `monitor.reason.${reason}` as MessageKey;
}

export interface KillSummaryProps {
  outcomes: readonly KillOutcome[];
}

/** The one-line honest report. Rendered after a run and after a dry run. */
export function KillSummary({ outcomes }: KillSummaryProps) {
  const { t, locale } = useI18n();
  const tally = tallyOutcomes(outcomes);
  const parts: string[] = [];
  if (tally.terminated > 0) {
    parts.push(t('monitor.kill.terminated', { n: formatNumber(tally.terminated, locale) }));
  }
  for (const refusal of tally.refused) {
    parts.push(
      t('monitor.kill.refused', {
        n: formatNumber(refusal.count, locale),
        reason: t(killReasonKey(refusal.reason)),
      }),
    );
  }
  if (tally.survived > 0) {
    parts.push(t('monitor.kill.survived', { n: formatNumber(tally.survived, locale) }));
  }
  if (parts.length === 0) parts.push(t('monitor.kill.nothing'));
  return <span className="s12">{parts.join(', ')}</span>;
}

/**
 * Whether the sheet's Run button is off. A health Clean up is confirmed only
 * against the target set Rust's dry run showed — never before it arrived.
 */
export function killRunDisabled(plan: KillPlanState): boolean {
  return plan.running || (plan.onlyHealthOrphans && !plan.preview);
}

/** Every pid the dry run says would be signalled, each with its name. */
export function KillDryRunTargets({ sets }: { sets: DryRunSets }) {
  const { t } = useI18n();
  return (
    <div className="col gap-2" data-testid="kill-dry-run-targets">
      <span className="muted">{t('monitor.kill.targets')}</span>
      {sets.targets.length > 0 ? (
        sets.targets.map((pid) => (
          <div className="row gap-6" key={pid}>
            <Mono small>{pid}</Mono>
            <span className="s12 ell">{sets.names[pid] ?? ''}</span>
          </div>
        ))
      ) : (
        <Mono small>{t('monitor.kill.targetsNone')}</Mono>
      )}
    </div>
  );
}

export function KillConfirmSheet() {
  const { t, locale } = useI18n();
  const plan = useAppStore((s) => s.proc.kill);
  const supervision = useAppStore((s) => s.proc.supervision);
  const closeKillSheet = useAppStore((s) => s.closeKillSheet);
  const setKillDryRun = useAppStore((s) => s.setKillDryRun);
  const runKill = useAppStore((s) => s.runKill);

  if (!plan.open) return null;

  const graceMs = supervision?.killGraceMs ?? FALLBACK_GRACE_MS;
  const sets = plan.preview ? dryRunSets(plan.preview) : null;
  const done = plan.outcomes !== null;

  return (
    <Sheet
      open={plan.open}
      title={t('monitor.kill.title', { n: formatNumber(plan.rows.length, locale) })}
      onClose={closeKillSheet}
      actions={
        <>
          <button type="button" className="btn" onClick={closeKillSheet}>
            {done ? t('common.close') : t('common.cancel')}
          </button>
          {done ? null : (
            <button
              type="button"
              className="btn primary"
              disabled={killRunDisabled(plan)}
              onClick={() => {
                setKillDryRun(false);
                void runKill();
              }}
            >
              {t('monitor.kill.run')}
            </button>
          )}
        </>
      }
    >
      <p className="s12 mon-sheet-lead">
        {t('monitor.kill.lead', {
          n: formatNumber(plan.rows.length, locale),
          grace: formatNumber(Math.round(graceMs / 1000), locale),
        })}
      </p>

      <div className="mon-sheet-targets">
        {plan.rows.map((row) => (
          <div className="mon-sheet-target" key={row.pid}>
            <Mono small>{row.pid}</Mono>
            <span className="s12 ell">{row.name}</span>
            <Mono small className="muted ell mon-sheet-argv" title={row.argvDisplay}>
              {row.argvDisplay}
            </Mono>
          </div>
        ))}
      </div>

      <ol className="mon-sheet-steps s11 muted">
        <li>{t('monitor.kill.step.term')}</li>
        <li>{t('monitor.kill.step.grace', { grace: formatNumber(Math.round(graceMs / 1000), locale) })}</li>
        <li>{t('monitor.kill.step.kill')}</li>
        <li>{t('monitor.kill.step.verify')}</li>
      </ol>

      <label className="row gap-6 s12 mon-sheet-dry">
        <input
          type="checkbox"
          checked={plan.dryRun}
          disabled={done || plan.running}
          onChange={(e) => setKillDryRun(e.currentTarget.checked)}
        />
        <span>{t('monitor.kill.dryRun')}</span>
      </label>

      {plan.running ? <span className="s12 muted">{t('common.loading')}</span> : null}

      {sets && !done ? (
        <div className="mon-sheet-sets s12">
          <KillDryRunTargets sets={sets} />
          {sets.excluded.length > 0 ? (
            <div className="col gap-2">
              <span className="muted">{t('monitor.kill.excluded')}</span>
              {sets.excluded.map((row) => (
                <div className="row gap-6" key={row.pid}>
                  <Mono small>{row.pid}</Mono>
                  <span className="s11 muted">{t(killReasonKey(row.reason))}</span>
                </div>
              ))}
            </div>
          ) : null}
        </div>
      ) : null}

      {plan.outcomes ? (
        <div className="mon-sheet-result col gap-4">
          <KillSummary outcomes={plan.outcomes} />
          {plan.outcomes.map((outcome, index) => (
            <div className="row gap-6 s11 muted" key={index}>
              <Mono small>
                {outcome.target.target === 'pid' ? outcome.target.pid : outcome.target.spawnId}
              </Mono>
              <span>{t(killReasonKey(outcome.report.fence))}</span>
              {outcome.report.refusedReason ? (
                <span>{t(killReasonKey(outcome.report.refusedReason))}</span>
              ) : null}
              {outcome.report.escalated.length > 0 ? (
                <span>
                  {t('monitor.kill.escalated', {
                    pids: outcome.report.escalated.join(' '),
                  })}
                </span>
              ) : null}
              {outcome.report.survivors.length > 0 ? (
                <span>
                  {t('monitor.kill.survivors', { pids: outcome.report.survivors.join(' ') })}
                </span>
              ) : null}
            </div>
          ))}
        </div>
      ) : null}

      {plan.error ? <span className="s12 mon-error">{t('error.internal')}</span> : null}
    </Sheet>
  );
}
