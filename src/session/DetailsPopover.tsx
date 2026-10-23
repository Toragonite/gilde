/**
 * `⋯ Details` (ux-ia §2.4, contract §3.4.4 `SessionDetails`, A10.4, A19.5,
 * A21.3). Facts only; paths are the core's `~/…` display strings. Rules:
 *  - spend: absent = unknown, never 0 (`formatTokens`/`formatCostMicros`
 *    return null → "unknown");
 *  - the compat state, the L2 connection, the teammate-version and
 *    teammate-binary lines and the approval-channel line are i18n keys;
 *  - the redacted command line is fetched on demand (`session_technical`)
 *    under "Technical details" and never kept.
 */

import { useId, useRef, useState } from 'react';

import { Popover, popoverTriggerProps } from '../components/Popover';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatBytes, formatCostMicros, formatDateTime, formatTimeOfDay, formatTokens } from '../lib/format';
import type { Session } from '../lib/gen/Session';
import type { SessionTechnicalRes } from '../lib/gen/SessionTechnicalRes';
import type { IpcError } from '../lib/ipcError';

const CONNECTION_KEY: Readonly<Record<string, MessageKey>> = {
  l1: 'details.connection.l1',
  l2: 'details.connectionHelper',
  pty: 'details.connection.pty',
  turns: 'details.connection.turns',
};

const MODE_KEY: Readonly<Record<string, MessageKey>> = {
  conversation: 'mode.conversation',
  terminal: 'mode.terminal',
  turnBased: 'mode.turnBased',
};

export function DetailsPopover({ session }: { session: Session }) {
  const { t, td, te, locale } = useI18n();
  const anchor = useRef<HTMLButtonElement>(null);
  const id = useId();
  const [open, setOpen] = useState(false);
  const [tech, setTech] = useState<{ kind: 'closed' } | { kind: 'loading' } | { kind: 'open'; res: SessionTechnicalRes } | { kind: 'error'; error: IpcError }>({ kind: 'closed' });
  const [revealError, setRevealError] = useState<IpcError | null>(null);
  const d = session.details;
  const unknown = t('common.unknown');

  const usage = d.usageToday ?? session.usageToday;
  const tokens = usage
    ? (() => {
        const input = formatTokens(usage.inputTokens, locale);
        const output = formatTokens(usage.outputTokens, locale);
        return input === null && output === null ? null : t('details.tokens', { input: input ?? unknown, output: output ?? unknown });
      })()
    : null;
  const cost = usage ? formatCostMicros(usage.costMicros, locale) : null;

  const rows: Array<[string, string]> = [];
  const add = (label: MessageKey, value: string | null | undefined) => {
    if (value !== null && value !== undefined && value !== '') rows.push([t(label), value]);
  };
  add('details.sessionId', d.vendorSessionId);
  add('details.account', d.accountLabel);
  add('details.configDir', d.configDir);
  add('details.folder', d.cwd);
  add('details.location', d.location);
  add('details.model', d.model ?? session.model);
  add('details.mode', d.controlMode ? t(MODE_KEY[d.controlMode] as MessageKey) : null);
  add('details.connection', d.connection ? t(CONNECTION_KEY[d.connection] as MessageKey) : null);
  add('details.version', d.claudeVersion);
  add('details.binary', d.binaryRealpath);
  add('details.started', d.startedAt ? formatDateTime(d.startedAt, locale) : null);
  add('details.memory', d.memoryBytes !== null ? formatBytes(d.memoryBytes, locale) : null);
  add('details.teamId', d.teamRawId);
  add('details.task', d.orchestratorTaskTitle);
  // Spend: shown always, "unknown" when no source reported (A10.4).
  rows.push([t('details.today'), [tokens, cost].filter(Boolean).join(' · ') || unknown]);

  const lines: string[] = [];
  if (d.compatState === 'unverified') lines.push(t('details.unverifiedVersion'));
  if (d.teamFormatUnverified) lines.push(t('details.teamFormatUnverified'));
  if (d.teammateVersionDiffers) lines.push(t('details.teammateVersionDiffers'));
  if (d.approvalChannelUnconfirmed) lines.push(t('convo.noPromptSeen', { tool: d.approvalChannelUnconfirmed }));
  if (d.followLastRowAt) lines.push(t('details.followNote', { time: formatTimeOfDay(d.followLastRowAt, locale) }));
  // Driver and pane lines arrive as the session's evidence line (A19.1, A21.3).
  const evidence = session.activity.evidence;
  if (evidence && evidence.key.startsWith('details.')) lines.push(td(evidence.key, evidence.params));

  const loadTech = () => {
    if (tech.kind === 'open' || tech.kind === 'loading') {
      setTech({ kind: 'closed' });
      return;
    }
    setTech({ kind: 'loading' });
    commands.session_technical({ uid: session.uid }).then(
      (res) => setTech({ kind: 'open', res }),
      (error: IpcError) => setTech({ kind: 'error', error }),
    );
  };

  return (
    <>
      <button
        ref={anchor}
        type="button"
        className="g-sbtn g-sbtn-icon"
        aria-label={t('session.details')}
        {...popoverTriggerProps(open, id)}
        onClick={() => setOpen((o) => !o)}
      >
        ⋯
      </button>
      <Popover open={open} anchor={anchor} onClose={() => setOpen(false)} id={id} label={t('session.details')} className="g-details">
        <dl className="g-details-list">
          {rows.map(([k, v]) => (
            <div key={k} className="g-details-row">
              <dt>{k}</dt>
              <dd className="mono">{v}</dd>
            </div>
          ))}
        </dl>
        {d.versionNote && <p className="g-details-line mono">{d.versionNote}</p>}
        {lines.map((l) => (
          <p key={l} className="g-details-line">
            {l}
          </p>
        ))}
        {d.transcriptPath && (
          <div className="g-details-row">
            <span>{t('details.record')}</span>
            <button
              type="button"
              className="g-sbtn"
              onClick={() =>
                commands.ide_reveal({ target: { kind: 'transcript', uid: session.uid } }).then(() => setRevealError(null), setRevealError)
              }
            >
              {t('details.reveal')}
            </button>
          </div>
        )}
        {revealError && <p className="g-details-line">{te(revealError)}</p>}
        {session.control.kind !== 'notRunning' && (
          <div className="g-details-tech">
            <button type="button" className="g-sbtn" aria-expanded={tech.kind === 'open'} onClick={loadTech}>
              {t('details.technical')} {tech.kind === 'open' ? '▾' : '▸'}
            </button>
            {tech.kind === 'loading' && <p className="g-details-line">{t('common.loading')}</p>}
            {tech.kind === 'error' && <p className="g-details-line">{te(tech.error)}</p>}
            {tech.kind === 'open' && (
              <dl className="g-details-list">
                <div className="g-details-row">
                  <dt>{t('details.pid')}</dt>
                  <dd className="mono">{tech.res.pid}</dd>
                </div>
                <div className="g-details-row">
                  <dt>{t('details.started')}</dt>
                  <dd className="mono">{formatDateTime(tech.res.startedAt, locale)}</dd>
                </div>
                <div className="g-details-row">
                  <dt>{t('details.command')}</dt>
                  <dd className="mono">{tech.res.commandRedacted.join(' ')}</dd>
                </div>
                <div className="g-details-row">
                  <dt>{t('details.parents')}</dt>
                  <dd className="mono">{tech.res.parentChain.join(' › ')}</dd>
                </div>
              </dl>
            )}
          </div>
        )}
      </Popover>
    </>
  );
}
