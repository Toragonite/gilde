/**
 * Evidence popover — why the pill says what it says (DESIGN §6-4, §9-4).
 *
 * Every `StatusEvidence` the engine produced is rendered as one human sentence
 * in the active locale, in the same words the Inspector uses (the sentence
 * builder is shared, so the two surfaces cannot word the same fact
 * differently). The pattern list is *declared fragile*: "시그니처 편집" opens
 * `agent-signatures.json`, because today's `(esc to interrupt)` will change and
 * the design absorbs that instead of pretending it will not.
 *
 * The `evidence` prop is `SessionView.evidence` — the same array the inspector
 * resolves through `Inspector.statusEvidenceOf` (BACKLOG M1.5 #4). Two panels,
 * one list; `src/terminal/statusEvidence.test.tsx` renders both and compares
 * them sentence by sentence.
 */

import { useEffect, useRef } from 'react';

import { evidenceSentence } from '../shell/Inspector';
import { StatusDot, nodeStatusOfSession, sessionStatusMessageKey } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import { asGildeError, cmd } from '../lib/commands';
import type { SessionStatus, StatusEvidence } from '../lib/ipc';
import { confidenceBand } from '../store/sessionSlice';
import { useAppStore } from '../store/useAppStore';

export interface EvidencePopoverProps {
  status: SessionStatus;
  confidence: number;
  evidence: readonly StatusEvidence[];
  onClose: () => void;
}

export function EvidencePopover({ status, confidence, evidence, onClose }: EvidencePopoverProps) {
  const { t, locale } = useI18n();
  const paths = useAppStore((s) => s.paths);
  const ensurePaths = useAppStore((s) => s.ensurePaths);
  const pushErrorToast = useAppStore((s) => s.pushErrorToast);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    void ensurePaths();
  }, [ensurePaths]);

  useEffect(() => {
    const onPointerDown = (event: MouseEvent) => {
      if (box.current && !box.current.contains(event.target as Node)) onClose();
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener('mousedown', onPointerDown, true);
    window.addEventListener('keydown', onKeyDown, true);
    return () => {
      window.removeEventListener('mousedown', onPointerDown, true);
      window.removeEventListener('keydown', onKeyDown, true);
    };
  }, [onClose]);

  const nodeStatus = nodeStatusOfSession(status);

  // `app_reveal` is the only file affordance M1's capability set exposes: it
  // selects `agent-signatures.json` in Finder, and a missing file (T16 writes
  // it on first run) is reported rather than swallowed.
  const openSignatures = () => {
    const file = paths?.signaturesFile;
    if (!file) return;
    void cmd.appReveal(file).catch((e) => pushErrorToast(asGildeError(e)));
  };

  return (
    <div className="popover evidence-popover col gap-4" ref={box} role="dialog" aria-label={t('inspector.section.evidence')}>
      <div className="row gap-8">
        <span className="strong s12">{t(sessionStatusMessageKey(status))}</span>
        <span className="muted s12">{t(`session.confidence.${confidenceBand(confidence)}`)}</span>
      </div>

      {evidence.length === 0 ? (
        <div className="muted s12">{t('inspector.noEvidence')}</div>
      ) : (
        evidence.map((item, i) => (
          <div className="row gap-8 evidence-row" key={`${item.kind}-${i}`}>
            <StatusDot status={nodeStatus} pulse={false} />
            <span>{evidenceSentence(item, t, locale)}</span>
          </div>
        ))
      )}

      <button
        type="button"
        className="linkish s12"
        onClick={openSignatures}
        disabled={!paths?.signaturesFile}
        title={paths?.signaturesFile ?? undefined}
      >
        {t('evidence.editSignatures')}
      </button>
    </div>
  );
}
