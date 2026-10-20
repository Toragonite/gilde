/**
 * The status pill and its confidence ring (DESIGN §6-4, §9-4).
 *
 * Two rules from the mockups (`docs/ui-mockups/Components.dc.html`, group 1):
 *
 *  - the ring is an SVG stroke whose arc length *is* the confidence, so the
 *    reader sees how sure the engine is without being handed a fake percentage;
 *  - **below 0.5 the ring is drawn hollow and dashed** — "we are guessing too"
 *    is shown, not hidden.
 *
 * Colour is never the only channel: the status text sits next to the ring, and
 * the four semantic tokens are the only colours in play (UI-GUIDELINES §2-1a).
 *
 * BACKLOG M1.5 #12: a *resting* session gets the dot, not the ring. An arc drawn
 * on a dashed track is the universal glyph for "loading", and next to the word
 * 유휴 it said the opposite of what was true — the session was waiting for the
 * user, not the user for the session. The mockup already draws it this way
 * (group 1: `working` pills carry the ring, `exited`/`done`/`error` pills carry a
 * dot), and the confidence survives the change: below 0.5 the dot is hollow and
 * dashed exactly as the ring was.
 */

import { useI18n } from '../i18n/useI18n';
import {
  StatusDot,
  nodeStatusOfSession,
  sessionStatusMessageKey,
  statusTone,
} from '../components/StatusDot';
import type { SessionStatus } from '../lib/ipc';
import { confidenceBand } from '../store/sessionSlice';

/** r = 4.5 in a 12×12 box, matching the mockup's `.pill .ring`. */
export const RING_RADIUS = 4.5;
export const RING_CIRCUMFERENCE = 2 * Math.PI * RING_RADIUS;

export interface RingDash {
  /** `stroke-dasharray` for the arc that shows the confidence. */
  arc: string;
  /** Hollow and dashed: the engine is not sure (confidence < 0.5). */
  hollow: boolean;
}

/** Pure: the ring geometry for a confidence in `0..1`. */
export function ringDash(confidence: number): RingDash {
  const clamped = Math.max(0, Math.min(1, Number.isFinite(confidence) ? confidence : 0));
  const filled = RING_CIRCUMFERENCE * clamped;
  return {
    arc: `${filled.toFixed(2)} ${(RING_CIRCUMFERENCE - filled).toFixed(2)}`,
    hollow: clamped < 0.5,
  };
}

/**
 * Which statuses earn the ring (BACKLOG M1.5 #12).
 *
 * The arc answers "how far along is this?", so it belongs to the three statuses
 * where something is in flight and the engine's certainty is worth reading.
 * Everything else — starting, idle, and the terminal states — is a resting
 * state, and a resting state that draws a spinner is a lie.
 */
export function showsConfidenceRing(status: SessionStatus): boolean {
  return (
    status.kind === 'working' || status.kind === 'waitingForInput' || status.kind === 'stalled'
  );
}

export interface SessionStatusPillProps {
  status: SessionStatus;
  confidence: number;
  /** Opens the evidence popover (DESIGN §9-4). */
  onClick?: () => void;
  expanded?: boolean;
}

export function SessionStatusPill({
  status,
  confidence,
  onClick,
  expanded,
}: SessionStatusPillProps) {
  const { t } = useI18n();
  const nodeStatus = nodeStatusOfSession(status);
  const tone = statusTone(nodeStatus);
  const { arc, hollow } = ringDash(confidence);
  const label = t(sessionStatusMessageKey(status));
  const title = `${label} · ${t(`session.confidence.${confidenceBand(confidence)}`)}`;

  const ring = showsConfidenceRing(status);

  const body = (
    <>
      {ring ? (
        <svg className="ring" viewBox="0 0 12 12" aria-hidden="true">
          <circle
            cx="6"
            cy="6"
            r={RING_RADIUS}
            fill="none"
            stroke="var(--n5)"
            strokeWidth="2"
            strokeDasharray={hollow ? '1.5 1.5' : undefined}
          />
          <circle
            cx="6"
            cy="6"
            r={RING_RADIUS}
            fill="none"
            stroke={`var(--st-${nodeStatus})`}
            strokeWidth="2"
            strokeDasharray={arc}
            transform="rotate(-90 6 6)"
          />
        </svg>
      ) : (
        <StatusDot status={nodeStatus} confidence={confidence} pulse={false} />
      )}
      <span>{label}</span>
    </>
  );

  if (!onClick) {
    return (
      <span className="pill" data-tone={tone} data-hollow={hollow ? 'true' : undefined} title={title}>
        {body}
      </span>
    );
  }

  return (
    <button
      type="button"
      className="pill"
      data-tone={tone}
      data-hollow={hollow ? 'true' : undefined}
      title={title}
      aria-expanded={expanded ? 'true' : 'false'}
      onClick={onClick}
    >
      {body}
    </button>
  );
}
