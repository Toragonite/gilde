/**
 * Status presentation — the one place a status becomes a colour, a label and a
 * confidence ring.
 *
 * UI-GUIDELINES §2-1a: there is no colour per status. Seven statuses map onto
 * the four semantic tokens plus the neutral scale, and the colour is always
 * accompanied by a text label, never used alone.
 */

import type { MessageKey } from '../i18n';
import type { NodeStatus, SessionStatus } from '../lib/ipc';

export type StatusTone = 'neutral' | 'ok' | 'warn' | 'error';

/** Session status (a tagged union) → graph node status (a flat enum). */
export function nodeStatusOfSession(status: SessionStatus): NodeStatus {
  switch (status.kind) {
    case 'starting':
      return 'idle';
    case 'idle':
      return 'idle';
    case 'working':
      return 'working';
    case 'waitingForInput':
      return 'waiting';
    case 'stalled':
      return 'stalled';
    case 'exited':
      return 'exited';
    case 'killed':
      return 'exited';
    case 'detached':
      return 'detached';
    case 'failed':
      return 'error';
  }
}

export function statusTone(status: NodeStatus): StatusTone {
  switch (status) {
    case 'waiting':
    case 'stalled':
    case 'dirty':
      return 'warn';
    case 'error':
    case 'orphan':
      return 'error';
    case 'done':
    case 'clean':
    case 'ok':
      return 'ok';
    default:
      return 'neutral';
  }
}

export function statusMessageKey(status: NodeStatus): MessageKey {
  return `status.${status}` as MessageKey;
}

export function sessionStatusMessageKey(status: SessionStatus): MessageKey {
  return status.kind === 'waitingForInput'
    ? 'session.status.waiting'
    : (`session.status.${status.kind}` as MessageKey);
}

export interface StatusDotProps {
  status: NodeStatus;
  /** Below 0.5 the dot is drawn hollow and dashed (DESIGN §9-5). */
  confidence?: number;
  /** `working` pulses at 1.2 s, `waiting` faster — it is the only state that needs a human. */
  pulse?: boolean;
  title?: string;
}

export function StatusDot({ status, confidence, pulse = true, title }: StatusDotProps) {
  const hollow = confidence !== undefined && confidence < 0.5;
  const animated = pulse && (status === 'working' || status === 'waiting');
  return (
    <span
      className="dot"
      data-status={status}
      data-hollow={hollow ? 'true' : undefined}
      data-pulse={animated ? 'true' : undefined}
      title={title}
    />
  );
}

export interface StatusPillProps {
  status: NodeStatus;
  label: string;
  confidence?: number;
  onClick?: () => void;
  title?: string;
}

/** Status text plus its dot. Clicking is how the evidence popover opens (T20). */
export function StatusPill({ status, label, confidence, onClick, title }: StatusPillProps) {
  const tone = statusTone(status);
  const content = (
    <>
      <StatusDot status={status} confidence={confidence} />
      <span>{label}</span>
    </>
  );
  if (!onClick) {
    return (
      <span className="pill" data-tone={tone} title={title}>
        {content}
      </span>
    );
  }
  return (
    <button type="button" className="pill" data-tone={tone} title={title} onClick={onClick}>
      {content}
    </button>
  );
}
