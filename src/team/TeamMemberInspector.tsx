/**
 * TeamMemberInspector — the inspector column on the Team view (F5): the details
 * of the selected member, which the roster rows leave out to stay two lines.
 *
 * The shell shows this column only while a member is selected
 * (`inspectorShownFor` in shell/AppShell.tsx); closing it deselects the member.
 * Read-only like the rest of the preview: it reads the store and sends nothing
 * to the orchestrator. Its one action is local — "Reveal in Finder" on the
 * member's worktree (`app_reveal`). The worktree block shows the path, the
 * branch (with how far it is ahead, when the snapshot carries it), the base
 * commit, and a warning when the worktree is gone or belongs to another
 * repository (repository identity from `team.identities`, `grouping.ts`).
 * Every engine string goes through `flat()`; codes are shown as words
 * (`model.ts`), process ids only in tooltips.
 */

import type { ReactNode } from 'react';

import { Badge } from '../components/Badge';
import { CloseIcon } from '../components/icons';
import { StatusPill } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import { asGildeError, cmd } from '../lib/commands';
import { formatNumber } from '../lib/format';
import { useAppStore } from '../store/useAppStore';
import { memberWorktreeFollow } from './grouping';
import { TeamMemberPanel } from './TeamMemberPanel';
import { reasonSentence } from './TeamPane';
import { CAP, flat, mailboxKey, memberPill, providerBadge } from './model';
import { noticeDisplay } from './notices';

function Row({ label, children, title }: { label: string; children: ReactNode; title?: string }) {
  return (
    <>
      <span className="k">{label}</span>
      <span className="v" title={title}>
        {children}
      </span>
    </>
  );
}

/** "Reveal in Finder": select the worktree folder; a failure (e.g. it is gone) is a toast. */
export function revealWorktree(path: string): Promise<void> {
  return cmd.appReveal(path).catch((e) => {
    useAppStore.getState().pushErrorToast(asGildeError(e));
  });
}

/** A commit for the eye: its first 12 characters (the full sha goes in the tooltip). */
export function shortSha(sha: string | null | undefined): string {
  return flat(sha, 64).slice(0, 12);
}

export function TeamMemberInspector({ children }: { children?: ReactNode }) {
  const { t, locale } = useI18n();
  const view = useAppStore((s) =>
    s.team.selectedTeamId === null ? undefined : s.team.views.get(s.team.selectedTeamId),
  );
  const selected = useAppStore((s) => s.team.selectedMember);
  const selectTeamMember = useAppStore((s) => s.selectTeamMember);
  const identities = useAppStore((s) => s.team.identities);
  const member = selected === null ? undefined : view?.state.members.find((m) => m.name === selected);

  if (!view || !member) {
    return (
      <aside className="inspector" aria-label={t('inspector.title')}>
        {children}
      </aside>
    );
  }

  const pill = memberPill(member, view.believed);
  const follow = memberWorktreeFollow(view, member, identities);
  const nf = (n: number) => formatNumber(n, locale);
  const used =
    member.modelUsed && member.modelUsed !== member.model ? ` → ${flat(member.modelUsed, CAP.label)}` : '';
  const model = member.model ? `${flat(member.model, CAP.label)}${used}` : t('team.modelDefault');
  const reason = member.reason ? reasonSentence(member.reason, t, locale) : null;
  const flags: string[] = [];
  if (member.sharesLeadPlan) flags.push(t('team.sharesLeadPlan'));
  if (member.machineLogin) flags.push(t('team.machineLogin'));

  return (
    <aside className="inspector" aria-label={t('team.member.details', { name: flat(member.name, CAP.name) })}>
      {children}
      <div className="hdr">
        <span className="strong s12 ell">{flat(member.name, CAP.name)}</span>
        <Badge>{providerBadge(member.provider)}</Badge>
        <StatusPill status={pill.status} label={pill.key ? t(pill.key) : pill.raw} />
        <span className="grow" />
        <button
          type="button"
          className="btn icon ghost"
          title={t('common.close')}
          aria-label={t('common.close')}
          onClick={() => selectTeamMember(null)}
        >
          <CloseIcon size="sm" />
        </button>
      </div>

      <div className="scroll-y inspector-body">
        {member.role ? <div className="sec">{flat(member.role, CAP.role)}</div> : null}
        {reason ? (
          <div className="s12 team-reason team-warn inspector-evidence">
            {reason.text}
            {reason.title ? <div className="muted">{reason.title}</div> : null}
          </div>
        ) : null}

        <div className="kv">
          <Row label={t('team.facts.account')}>{flat(member.accountLabel || member.worker, CAP.label)}</Row>
          <Row label={t('team.facts.model')}>{model}</Row>
          <Row label={t('team.facts.messages')} title={noticeDisplay(member.mailboxReason, t, 80) || undefined}>
            {t(mailboxKey(member.mailbox))}
          </Row>
          <Row label={t('team.facts.commands')}>
            {member.runsCommands ? t('team.runsCommands') : t('team.noCommands')}
          </Row>
          {member.elevated ? (
            <Row label={t('team.facts.elevated')}>
              <span className="team-warn">{flat(member.elevated, 32)}</span>
            </Row>
          ) : null}
          {member.billableOverage ? (
            <Row label={t('team.facts.billing')}>
              <span className="team-warn">{t('team.billableOverage')}</span>
            </Row>
          ) : null}
          {flags.length > 0 ? <Row label={t('team.facts.login')}>{flags.join(' · ')}</Row> : null}
          {member.worktree.path ? (
            <Row label={t('inspector.worktree')} title={flat(member.worktree.path, CAP.tip)}>
              <span className="col gap-2 team-insp-worktree">
                <span className="mono team-insp-path">{flat(member.worktree.path, CAP.tip)}</span>
                {follow === 'missing' || follow === 'other' ? (
                  <span className="team-warn" data-follow={follow}>
                    {t(follow === 'missing' ? 'team.worktree.missing' : 'team.worktree.other')}
                  </span>
                ) : null}
                <button
                  type="button"
                  className="s12 team-insp-reveal"
                  onClick={() => void revealWorktree(member.worktree.path)}
                >
                  {t('team.worktree.reveal')}
                </button>
              </span>
            </Row>
          ) : null}
          {member.worktree.branch ? (
            <Row
              label={t('inspector.branch')}
              title={flat(member.worktree.branch, CAP.tip) || undefined}
            >
              <span className="mono">{flat(member.worktree.branch, CAP.label)}</span>
              {member.worktree.ahead !== undefined && member.worktree.ahead > 0
                ? ` · ${t('team.ahead', { n: member.worktree.ahead })}`
                : ''}
              {member.worktree.dirty ? <span className="team-warn"> · {t('team.dirty')}</span> : null}
            </Row>
          ) : null}
          {member.worktree.baseCommit ? (
            <Row label={t('team.facts.base')} title={flat(member.worktree.baseCommit, 64)}>
              <span className="mono">{shortSha(member.worktree.baseCommit)}</span>
            </Row>
          ) : null}
          {member.turn && view.believed ? (
            <Row
              label={t('team.facts.turn')}
              title={member.turn.pid !== undefined ? t('team.host.pid', { pid: member.turn.pid }) : undefined}
            >
              {t('team.turn', { n: member.turn.n })}
            </Row>
          ) : null}
          <Row label={t('inspector.tokens')}>
            {t('team.tokens', { in: nf(member.tokens.in), out: nf(member.tokens.out) })}
          </Row>
          {member.pending > 0 ? <Row label={t('team.facts.waiting')}>{t('team.pending', { n: member.pending })}</Row> : null}
          {member.tasks.length > 0 ? (
            <Row label={t('team.facts.tasks')}>
              <span className="mono">{member.tasks.map((id) => flat(id, 8)).join(', ')}</span>
            </Row>
          ) : null}
        </div>

        <TeamMemberPanel view={view} />
      </div>
    </aside>
  );
}
